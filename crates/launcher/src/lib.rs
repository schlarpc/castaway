//! The launcher: the one binary on the panel that never changes.
//!
//! The box's scheduled task points at `launcher.exe` and at nothing else, so everything
//! underneath it — the receiver, its browser, its DLLs — can be replaced by an unattended
//! update without touching the one thing Windows persistence knows about. That is the
//! whole reason this crate exists and it is why it is deliberately boring: the launcher
//! does not update itself in the normal flow, and it has no network, no config file, and
//! no state beyond two pointer files.
//!
//! What it does, in order (#342):
//!
//! - read `current.txt`, spawn `versions/<sha>/castaway.exe`, hold it in a **job object**
//!   with kill-on-close so an Electron child cannot outlive it;
//! - restart it on exit, forever, with capped backoff, because it is a kiosk;
//! - treat one reserved exit code as "re-read `current.txt` first" — which is how an
//!   update activates, and why activation is always "exit and let the launcher respawn
//!   me" rather than spawning a successor (a successor would land outside the interactive
//!   session, docs/cross-build.md);
//! - roll back to `previous.txt` when a version that has *never* reported itself healthy
//!   keeps dying young.
//!
//! The split follows ground rule 3. [`supervise`] is the decision — a pure function of
//! what the last run did, with every constant assertable in virtual time — and
//! `src/main.rs` is the thin shell that spawns processes, reads the clock once per run,
//! and does what it is told. The layout both this and the updater agree about lives in
//! [`castaway_paths::install`], because a launcher and an updater that disagree about
//! where `current.txt` is are a panel that does not come back up.
//!
//! It builds and runs on Linux too, and that is not an accident: it is what lets the
//! whole loop — crash, back off, strike out, roll back — be driven by a test with no
//! Windows in sight (ground rule 5). Only the job object is `cfg(windows)`.

#![forbid(unsafe_code)]

pub mod supervise;

pub use supervise::{Next, Run, Stopped, Supervisor, ACTIVATE_EXIT_CODE};
