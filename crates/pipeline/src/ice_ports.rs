//! The UDP ports ICE is allowed to bind (#18).
//!
//! Not an optimisation and not tidiness. `crates/app/src/surface.rs` generates
//! `nix/network-surface.json`, the NixOS module derives the firewall from it, and
//! `clippy.toml` denies raw bind calls outside registered sites — so a socket outside the
//! declared range is one the deployed box silently drops. The connection would negotiate
//! perfectly and then carry nothing, which is the worst shape a networking bug can have.
//!
//! Handing webrtc-rs an explicit address per peer is also what keeps the bind inside a
//! range we named: the crate binds it, but it binds what it is told.

use std::collections::HashSet;
use std::sync::Mutex;

/// The ports `[remote.ice_ports]` allows, and which are in use.
#[derive(Debug)]
pub struct PortPool {
    first: u16,
    last: u16,
    taken: Mutex<HashSet<u16>>,
}

impl PortPool {
    /// A pool over the inclusive range `first..=last`.
    ///
    /// A reversed range is empty rather than an error: it comes from an operator's config
    /// file, and the honest failure is "no port is free" at the moment a peer tries,
    /// which says so in the log, rather than a panic at startup.
    #[must_use]
    pub fn new(first: u16, last: u16) -> Self {
        Self {
            first,
            last,
            taken: Mutex::new(HashSet::new()),
        }
    }

    /// Claim the lowest free port, or `None` if every one is in use.
    ///
    /// Lowest-free-first, like `[media_ports]`: a predictable allocation makes a packet
    /// capture readable, and there is nothing to gain from spreading peers over the range.
    pub fn take(&self) -> Option<u16> {
        let mut taken = self.taken.lock().ok()?;
        let port = (self.first..=self.last).find(|port| !taken.contains(port))?;
        taken.insert(port);
        Some(port)
    }

    /// Return a port to the pool.
    ///
    /// Returning one that was never taken is a no-op rather than a panic: the release
    /// path is reached from a peer's state machine and from its media pump ending, and
    /// which of the two arrives first is not ours to decide.
    pub fn give_back(&self, port: u16) {
        if let Ok(mut taken) = self.taken.lock() {
            taken.remove(&port);
        }
    }

    /// How many are in use.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.taken.lock().map_or(0, |taken| taken.len())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ports_come_out_lowest_first_and_go_back() {
        let pool = PortPool::new(41032, 41034);
        assert_eq!(pool.take(), Some(41032));
        assert_eq!(pool.take(), Some(41033));
        assert_eq!(pool.in_use(), 2);
        pool.give_back(41032);
        assert_eq!(pool.take(), Some(41032), "the freed one is reused");
    }

    #[test]
    fn an_exhausted_pool_says_so_rather_than_wrapping() {
        // Handing out a port already in use would bind a second socket onto a live peer's
        // — or fail the bind deep inside webrtc-rs, which is a much worse place to learn.
        let pool = PortPool::new(41032, 41033);
        assert!(pool.take().is_some());
        assert!(pool.take().is_some());
        assert_eq!(pool.take(), None);
    }

    #[test]
    fn returning_a_port_twice_is_harmless() {
        // The release path is reached from the connection state machine and from the
        // media pump ending, in either order.
        let pool = PortPool::new(41032, 41033);
        let port = pool.take().unwrap();
        pool.give_back(port);
        pool.give_back(port);
        assert_eq!(pool.in_use(), 0);
        assert_eq!(pool.take(), Some(port));
    }

    #[test]
    fn returning_a_port_that_was_never_taken_is_harmless() {
        let pool = PortPool::new(41032, 41033);
        pool.give_back(9999);
        assert_eq!(pool.in_use(), 0);
    }

    #[test]
    fn a_reversed_range_is_empty_rather_than_a_panic() {
        // It comes from an operator's config file.
        let pool = PortPool::new(41063, 41032);
        assert_eq!(pool.take(), None);
    }

    #[test]
    fn a_single_port_range_holds_one_peer() {
        let pool = PortPool::new(41032, 41032);
        assert_eq!(pool.take(), Some(41032));
        assert_eq!(pool.take(), None);
    }
}
