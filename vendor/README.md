# `vendor/`

Third-party source we carry patched, because upstream cannot yet do something this
receiver needs. Everything here is a **temporary** divergence: each entry exists to be
deleted once the change lands upstream, and each carries the patch as a standalone file
so re-applying it to a newer release is mechanical rather than archaeological.

## `mdns-sd` 0.20.3 — several DNS-SD sub-types on one instance (#227)

**What upstream does.** `ServiceInfo` holds one `sub_domain: Option<String>`, and
`ServiceDaemon`'s registry is a `HashMap` keyed by a fullname that `ServiceInfo::new`
computes with the sub-type *stripped*. Registering one instance under several sub-types
therefore silently keeps only the last: every `register()` returns `Ok`, and every record
but one never reaches the wire. Measured with tcpdump against our own responder, then
predicted and confirmed — querying the last-registered sub-type is answered, querying any
earlier one is not.

**Why it matters here.** Play Services matches a discovered device to its filter criteria
out of the sub-types in the mDNS answer and nothing else — not from
`GET_APP_AVAILABILITY`, which it also asks and which we already answered correctly. A
receiver serving seven Cast application ids but advertising one is invisible to the
pickers filtering on the other six (#226).

**What the patch does.** Adds `sub_domains: Vec<String>` beside the existing
`sub_domain`, which stays the *first* of them so every existing accessor keeps its
meaning; matches a query against any of them; and emits one PTR per sub-type at the three
places that previously emitted one. Additive and backwards-compatible: 58 added lines
across three files, no public API removed.

**Upstream status.** Unreported. The closest is
[keepsimple1/mdns-sd#145](https://github.com/keepsimple1/mdns-sd/issues/145), which is the
*browse* half of the same root cause — the maintainer's note on
[#146](https://github.com/keepsimple1/mdns-sd/pull/146) diagnoses it exactly: "we only
store mappings from PTR to instance SRV, not the other way. Both parent type PTR and
subtype PTR points to the same instance SRV." The patch is shaped to be offered rather
than kept.

### Re-applying on an upstream bump

```sh
# fetch the new release, then:
patch -p1 -d vendor/mdns-sd < vendor/mdns-sd.patch
```

If it no longer applies, the three touched sites are `ServiceInfo`'s fields and
`matches_type_or_subtype` in `src/service_info.rs`, and the `get_subtype()` PTR emission
in `src/dns_parser.rs` and `src/service_daemon.rs` (two sites). Regenerate the patch with
`diff -u` against the pristine release afterwards.
