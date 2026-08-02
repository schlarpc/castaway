//! Stitching entity references back into element text.
//!
//! quick-xml 0.38 stopped inlining `&amp;` and `&#65;` into the surrounding `Event::Text`
//! and began emitting each one as its own `Event::GeneralRef`. A run of character data
//! therefore arrives in fragments, and a reader that still treats the first `Event::Text`
//! as the whole value silently truncates every string at its first entity — `AT&amp;T`
//! reads as `AT`, and a URL loses everything past its first `&amp;`. Both XML readers in
//! this crate accumulate across fragments and use this to turn each reference back into
//! the text it stands for.
//!
//! There is no reader option that restores the old inlining; the fragments are the API.
//!
//! `proto-gamestream` carries its own copy of this for its NVHTTP reader: `proto-*` crates
//! do not depend on each other, and one small helper does not justify a substrate crate.

use quick_xml::events::BytesRef;

/// The replacement text for one character reference or predefined general reference.
///
/// `None` for a reference this parser cannot resolve — an undeclared entity such as
/// `&nbsp;`, which is malformed XML rather than something with a defined expansion.
/// Callers decide whether that is fatal, because they disagreed before quick-xml stopped
/// resolving entities itself: the DIDL reader dropped the field and kept the document,
/// the SOAP reader rejected the body.
pub(crate) fn resolve(r: &BytesRef<'_>) -> Option<String> {
    match r.resolve_char_ref() {
        Ok(Some(c)) => Some(c.to_string()),
        Ok(None) => {
            let name = r.decode().ok()?;
            quick_xml::escape::resolve_predefined_entity(&name).map(str::to_owned)
        }
        Err(_) => None,
    }
}
