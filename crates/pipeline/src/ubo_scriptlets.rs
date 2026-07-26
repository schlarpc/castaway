//! Turning uBlock Origin's scriptlet modules into engine resources — by running them.
//!
//! uBO's scriptlets are ES modules that end in a registration:
//!
//! ```text
//! registerScriptlet(jsonPrune, { name: 'json-prune.js', dependencies: [ safeSelf ] });
//! ```
//!
//! and `scriptlets.js` additionally carries older-style entries pushed onto the same
//! array, where the function arrives as a `fn:` field and dependencies are already names.
//! Both forms land in one place: `builtinScriptlets`, which uBO exports.
//!
//! **This module evaluates that graph rather than parsing it.** The first attempt was a
//! text scanner, and it was wrong three times in an afternoon — multi-line imports, the
//! second registration form, and a brace-matching failure inside a real function body —
//! each time silently, producing fewer resources rather than an error. Running uBO's own
//! code cannot disagree with uBO: `fn.toString()` gives exact source, and the dependency
//! names are the ones `base.js` resolved itself. A refactor upstream breaks this only if
//! it breaks uBO too.
//!
//! The engine already understood the output — `fn/javascript` is uBO's own invention for
//! dependency functions, and `adblock` models it. Only the reading was missing.

use adblock::resources::{MimeType, Resource, ResourceType};
use base64::Engine as _;
use rquickjs::loader::{BuiltinLoader, BuiltinResolver};
use rquickjs::{Context, Module, Runtime};
use serde::Deserialize;

use crate::error::PipelineError;

/// The module every scriptlet is reachable from, as a path under uBO's `src/js/`.
pub const ENTRY_MODULE: &str = "resources/scriptlets.js";

/// uBO's convention: a dependency function's registered name ends in `.fn`, a scriptlet's
/// in `.js`. That distinction becomes the resource's MIME type, and the engine refuses to
/// inject anything that is not `application/javascript` — so getting it backwards means
/// every rule matches and injects nothing.
const DEPENDENCY_SUFFIX: &str = ".fn";

/// What uBO records for each registration.
#[derive(Debug, Deserialize)]
struct Registered {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    /// `Function.prototype.toString()` of the scriptlet — `None` if the entry somehow
    /// carries no function, which is not something to inject.
    source: Option<String>,
}

/// Evaluate uBO's module graph and convert its registry into engine resources.
///
/// `modules` is `(path, source)`, where the path is relative to uBO's `src/js/` — that is
/// what makes `../urlskip.js` from `resources/href-sanitizer.js` resolve the way it does
/// in a browser. [`ENTRY_MODULE`] must be among them.
///
/// # Errors
/// [`PipelineError::Scriptlets`] if the graph fails to evaluate (a missing module, or a
/// shape uBO has moved to that no longer exposes `builtinScriptlets`). That is a real
/// error rather than an empty result on purpose: an empty scriptlet set is indisting-
/// uishable from a working one until someone notices ads.
pub fn convert(modules: &[(String, String)]) -> Result<Vec<Resource>, PipelineError> {
    let mut resolver = BuiltinResolver::default();
    let mut loader = BuiltinLoader::default();
    for (name, source) in modules {
        resolver.add_module(name.clone());
        loader.add_module(name.clone(), source.clone());
    }

    let runtime = Runtime::new().map_err(|e| PipelineError::Scriptlets(e.to_string()))?;
    runtime.set_loader(resolver, loader);
    let context = Context::full(&runtime).map_err(|e| PipelineError::Scriptlets(e.to_string()))?;

    let dumped = context.with(|ctx| -> Result<String, PipelineError> {
        // Ask uBO for its own registry. `fn.toString()` is the point of doing this in a
        // JS engine at all: it is the function's exact source, including whatever syntax
        // upstream is using this month.
        let entry = format!(
            r"
            import {{ builtinScriptlets }} from '{ENTRY_MODULE}';
            globalThis.__castaway_scriptlets = JSON.stringify(builtinScriptlets.map(d => ({{
                name: d.name,
                aliases: d.aliases || [],
                dependencies: d.dependencies || [],
                source: typeof d.fn === 'function' ? d.fn.toString() : null,
            }})));
            "
        );
        let evaluated = Module::evaluate(ctx.clone(), "castaway-entry.js", entry)
            .map_err(|e| PipelineError::Scriptlets(describe(&ctx, &e)))?;
        evaluated
            .finish::<()>()
            .map_err(|e| PipelineError::Scriptlets(describe(&ctx, &e)))?;
        ctx.globals()
            .get::<_, String>("__castaway_scriptlets")
            .map_err(|e| PipelineError::Scriptlets(e.to_string()))
    })?;

    let registered: Vec<Registered> =
        serde_json::from_str(&dumped).map_err(|e| PipelineError::Scriptlets(e.to_string()))?;

    Ok(registered
        .into_iter()
        .filter_map(|entry| {
            let source = entry.source?;
            Some(Resource {
                kind: if entry.name.ends_with(DEPENDENCY_SUFFIX) {
                    ResourceType::Mime(MimeType::FnJavascript)
                } else {
                    ResourceType::Mime(MimeType::ApplicationJavascript)
                },
                name: entry.name,
                aliases: entry.aliases,
                content: base64::prelude::BASE64_STANDARD.encode(source),
                dependencies: entry.dependencies,
                permission: adblock::resources::PermissionMask::default(),
            })
        })
        .collect())
}

/// A JS exception says which module failed to resolve and why; the Rust error alone does
/// not, and that difference is most of the debugging.
fn describe(ctx: &rquickjs::Ctx<'_>, error: &rquickjs::Error) -> String {
    let caught = ctx.catch();
    caught.as_exception().map_or_else(
        || error.to_string(),
        |exception| {
            format!(
                "{}: {}",
                error,
                exception.message().unwrap_or_else(|| "?".to_string())
            )
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn decoded(resources: &[Resource], name: &str) -> String {
        let r = resources.iter().find(|r| r.name == name).unwrap();
        String::from_utf8(base64::prelude::BASE64_STANDARD.decode(&r.content).unwrap()).unwrap()
    }

    /// A miniature of uBO's real tree: `base.js` holding the registry, a dependency in one
    /// module, a scriptlet in another, and an index that pulls them together and also
    /// pushes an entry in the older style.
    fn tree() -> Vec<(String, String)> {
        vec![
            (
                "resources/base.js".into(),
                r"
export const registeredScriptlets = [];
export const registerScriptlet = (fn, details) => {
    details.fn = fn;
    fn.details = details;
    if ( Array.isArray(details.dependencies) ) {
        details.dependencies.forEach((fn, i, array) => {
            if ( typeof fn !== 'function' ) { return; }
            array[i] = fn.details.name;
        });
    }
    registeredScriptlets.push(details);
};
"
                .into(),
            ),
            (
                "resources/safe-self.js".into(),
                r"
import { registerScriptlet } from './base.js';
export function safeSelf() {
    const re = /\{[^}]*\}/g;
    const s = `a } brace ${ { nested: '}' } } inside`;
    return [re, s];
}
registerScriptlet(safeSelf, { name: 'safe-self.fn' });
"
                .into(),
            ),
            (
                "resources/json-prune.js".into(),
                r"
import { registerScriptlet } from './base.js';
import {
    safeSelf,
} from './safe-self.js';
export function jsonPrune(paths = '') {
    return safeSelf();
}
registerScriptlet(jsonPrune, {
    name: 'json-prune.js',
    aliases: [ 'jp.js' ],
    dependencies: [ safeSelf ],
});
"
                .into(),
            ),
            (
                "resources/scriptlets.js".into(),
                r"
import { registeredScriptlets } from './base.js';
import './json-prune.js';
export const builtinScriptlets = registeredScriptlets;

function trustedReplaceFetchResponse() { return 'trusted'; }
builtinScriptlets.push({
    name: 'trusted-replace-fetch-response.js',
    requiresTrust: true,
    aliases: [ 'trusted-rpfr.js' ],
    fn: trustedReplaceFetchResponse,
    dependencies: [ 'safe-self.fn' ],
});
"
                .into(),
            ),
        ]
    }

    #[test]
    fn both_registration_forms_arrive_with_their_metadata() {
        let resources = convert(&tree()).unwrap();

        let scriptlet = resources
            .iter()
            .find(|r| r.name == "json-prune.js")
            .unwrap();
        assert_eq!(
            scriptlet.kind,
            ResourceType::Mime(MimeType::ApplicationJavascript),
            "a scriptlet has to be injectable; the engine refuses any other kind"
        );
        assert_eq!(scriptlet.aliases, vec!["jp.js"]);
        assert_eq!(
            scriptlet.dependencies,
            vec!["safe-self.fn"],
            "uBO turns dependency *functions* into names at registration; running its own \
             code is what gets that for free"
        );

        // The older form, which is where every `trusted-` scriptlet lives.
        let trusted = resources
            .iter()
            .find(|r| r.name == "trusted-replace-fetch-response.js")
            .expect("builtinScriptlets.push entries are registrations too");
        assert_eq!(trusted.aliases, vec!["trusted-rpfr.js"]);
        assert_eq!(trusted.dependencies, vec!["safe-self.fn"]);

        let dependency = resources.iter().find(|r| r.name == "safe-self.fn").unwrap();
        assert_eq!(
            dependency.kind,
            ResourceType::Mime(MimeType::FnJavascript),
            "a `.fn` registration is a dependency, not something a rule can name"
        );
    }

    #[test]
    fn function_sources_arrive_exactly_and_start_where_the_engine_looks() {
        let resources = convert(&tree()).unwrap();
        let source = decoded(&resources, "json-prune.js");
        // The engine reads the function name off the front to invoke it.
        assert!(source.starts_with("function jsonPrune("), "{source}");
        assert!(!source.contains("registerScriptlet"), "{source}");

        // The case that defeated the hand-written scanner this replaced: braces inside a
        // regex and a template literal with a nested object.
        let tricky = decoded(&resources, "safe-self.fn");
        assert!(
            tricky.contains("return [re, s];"),
            "body was cut short: {tricky}"
        );
    }

    #[test]
    fn a_graph_that_does_not_evaluate_is_an_error_not_an_empty_set() {
        // An empty scriptlet set looks exactly like a working one until someone notices
        // ads, so this must be loud.
        let broken = vec![(
            "resources/scriptlets.js".into(),
            "import './nowhere.js'; export const builtinScriptlets = [];".into(),
        )];
        let err = match convert(&broken) {
            Err(e) => e,
            Ok(r) => panic!("a broken graph must not look like {} resources", r.len()),
        };
        assert!(
            format!("{err}").contains("nowhere.js"),
            "the error should name the module that could not be resolved: {err}"
        );
    }
}
