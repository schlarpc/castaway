//! The expiry canary: fails while the checked-in signature tables still have time
//! left, so a lapsing table arrives as a red check instead of a panel that quietly
//! stops answering official senders (#294, extracted from #40).
//!
//! Each borrowed identity ships a signature table that is a *floor* with a fixed end
//! date. The live endpoints keep the identities rolling in normal operation, but a
//! panel that has lost its uplink falls back to the table, and past the table's
//! horizon there is nothing left to fall back to — Cast simply stops working, with
//! nothing in the logs to say why, because "the calendar moved past the build" is not
//! a runtime error.
//!
//! This is deliberately not gated on a carved identity: [`CksTable::HORIZON_UNIX`] and
//! [`AirServerTable::HORIZON_UNIX`] are compile-time constants derived from each
//! table's window layout, so the canary reads them on any build — a plain
//! `cargo nextest run -p cast-replay`, `nix flake check`'s `test` derivation, or CI —
//! rather than only where the fixtures happen to be present. That is the whole point:
//! the check must be able to go red everywhere the code is built.
//!
//! Ground rule 3 / the time-in-tests rule: the comparison lives in
//! [`cast_replay::days_until`], pure over `(horizon, today)`. This test reads the
//! clock exactly once, here at its boundary, and passes the day inward.

use cast_replay::canary::days_until;
use cast_replay::{AirServerTable, CksTable, MIN_REMAINING_DAYS};
use std::time::{SystemTime, UNIX_EPOCH};

/// Read today once, at the boundary. Everything downstream is pure over this value.
fn today_unix() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_secs();
    i64::try_from(secs).expect("a Unix timestamp in seconds fits in i64")
}

#[test]
fn cks_table_has_at_least_a_quarter_of_runway_left() {
    let now = today_unix();
    let days = days_until(CksTable::HORIZON_UNIX, now);
    assert!(
        days >= MIN_REMAINING_DAYS,
        "The checked-in CKS (AirReceiver) signature table has {days} days left \
         (threshold {MIN_REMAINING_DAYS}); it stops at Unix {} = 2027-12-06. Past that \
         a panel on the offline table stops authenticating to official senders, \
         silently.\n\
         What to do: re-export the CKS table from a fresh AirReceiver artifact \
         (crates/cast-replay/PROVENANCE.md, nix/airreceiver-carve.nix) and raise \
         cks::WINDOW_COUNT, or provision a Cast credential of our own — the durable fix \
         #40 is open for. This canary is #294; the owner is whoever holds the \
         cast-replay fixtures.",
        CksTable::HORIZON_UNIX,
    );
}

#[test]
fn airserver_table_has_at_least_a_quarter_of_runway_left() {
    let now = today_unix();
    let days = days_until(AirServerTable::HORIZON_UNIX, now);
    assert!(
        days >= MIN_REMAINING_DAYS,
        "The checked-in AirServer signature table has {days} days left \
         (threshold {MIN_REMAINING_DAYS}); it stops at Unix {} = 2027-03-21 — eight \
         months before CKS's, so this is the table the canary trips on first. Past it a \
         panel on the offline table stops authenticating to official senders, silently.\n\
         What to do: re-export the AirServer table from a fresh AirServer database \
         (crates/cast-replay/PROVENANCE.md, nix/airserver-carve.nix) and raise \
         airserver::WINDOW_COUNT, or provision a Cast credential of our own (#40). This \
         canary is #294; the owner is whoever holds the cast-replay fixtures.",
        AirServerTable::HORIZON_UNIX,
    );
}
