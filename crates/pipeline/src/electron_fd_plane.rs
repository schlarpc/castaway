//! The fd plane: receiving the browser's frame descriptors over `SCM_RIGHTS` (#271).
//!
//! The production transport for D36's painted frames. The spike shipped with
//! `pidfd_getfd(2)` (`hwaccel::remote_handle`), which works exactly as long as the
//! kernel's ptrace policy says it may: `kernel.yama.ptrace_scope = 1` permits it for a
//! descendant and nothing else, `2` demands `CAP_SYS_PTRACE`, `3` refuses everyone. A
//! hardened box — or any future arrangement where the browser is not our child — turns
//! every frame into a black panel with a message that sounds like a GPU problem.
//! Passing the descriptor itself depends on nothing but the socket carrying it.
//!
//! ## The shape, and why it is a second socket
//!
//! The control socket's writer is *JavaScript*: `main.js` writes lines through Node's
//! buffered `net` stream, and Node may hold a partial write. A native `sendmsg` on the
//! same fd could land ancillary data in the middle of a buffered line — rare,
//! load-dependent framing corruption, the exact class `browser_proto`'s framer exists
//! to rule out. So the descriptors travel on a socket of their own, bound beside the
//! control socket and connected once at startup by the host app's native piece
//! (`castaway-browser-fd`); the control socket stays a pure text protocol.
//!
//! One message is one `sendmsg`: an 8-byte little-endian paint id as payload, the
//! plane fds as `SCM_RIGHTS`. The id rides *with* the rights because the kernel
//! delivers ancillary data attached to its bytes and never coalesces reads across a
//! control-message boundary — an id can therefore never be paired with another
//! message's descriptors.
//!
//! ## Ordering against the paint message
//!
//! `main.js` calls the addon **before** writing the paint line, and `sendmsg` on a
//! Unix socket copies into the receiver's buffer synchronously — so by the time the
//! control socket reader sees `fdTransport: "scm"`, the descriptors are at worst one
//! thread-schedule away. [`FdTable::take`] still waits with a deadline rather than
//! asserting that, and a miss costs the frame (released, logged), never the session.

#![allow(unsafe_code)] // recvmsg + cmsg walking; each unsafe block carries its SAFETY.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// The most fds one message may carry — mirrored by `MAX_FDS` in `castaway-browser-fd`,
/// which is the side that enforces it. Four covers every plane layout Chromium emits.
pub(crate) const MAX_FDS: usize = 4;

/// How many unclaimed deliveries to hold before evicting the oldest.
///
/// A delivery is normally claimed by the very next control-socket message, so this
/// depth only fills if the browser sends fds and then dies before the paint line — and
/// each entry holds live descriptors, which is exactly what must not accumulate.
const MAX_UNCLAIMED: usize = 32;

/// Descriptors that have arrived and not yet been claimed by their paint message.
///
/// Keyed by paint id, which `main.js` allocates monotonically — so "oldest" is
/// `pop_first`, and eviction cannot outlive a delivery that is still plausibly wanted.
#[derive(Debug, Default)]
pub(crate) struct FdTable {
    arrived: Mutex<BTreeMap<u64, Vec<OwnedFd>>>,
    signal: Condvar,
}

impl FdTable {
    /// Record one delivery. Evicts (and thereby closes) the oldest if the browser is
    /// somehow sending descriptors nothing claims.
    pub(crate) fn insert(&self, id: u64, fds: Vec<OwnedFd>) {
        let mut arrived = match self.arrived.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        while arrived.len() >= MAX_UNCLAIMED {
            if let Some((stale, _)) = arrived.pop_first() {
                warn!(target: "castaway::browser", id = stale, "fd plane: evicting unclaimed descriptors");
            }
        }
        arrived.insert(id, fds);
        drop(arrived);
        self.signal.notify_all();
    }

    /// Claim the descriptors for `id`, waiting up to `wait` for a delivery that is
    /// still in flight. `None` after the deadline — the caller drops that frame.
    pub(crate) fn take(&self, id: u64, wait: Duration) -> Option<Vec<OwnedFd>> {
        let deadline = Instant::now() + wait;
        let mut arrived = match self.arrived.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if let Some(fds) = arrived.remove(&id) {
                return Some(fds);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            arrived = match self.signal.wait_timeout(arrived, deadline - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// How many deliveries are waiting, for tests.
    #[cfg(test)]
    fn unclaimed(&self) -> usize {
        match self.arrived.lock() {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// Bind the fd-plane listener beside the control socket.
///
/// A plain `std` Unix listener rather than `interprocess`, because the whole point of
/// this socket is `recvmsg` with control-message space, which only the raw fd can do.
/// Not a registered network surface: like the control socket it is a filesystem-scoped
/// local socket, never a port the firewall could see.
pub(crate) fn bind(address: &str) -> std::io::Result<(String, UnixListener)> {
    let path = format!("{address}-fd");
    // A socket file left by a killed receiver would otherwise fail the next bind.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    Ok((path, listener))
}

/// Accept the host app's one fd-plane connection and receive until it closes.
///
/// On its own thread for the same reason the control reader is: descriptors must be
/// drained as they arrive, not at frame rate. The browser never connecting — no addon
/// found, an older host app — is not an error: the accept gives up at `deadline` and
/// the paint path keeps using `pidfd_getfd`. Either way the socket file is unlinked on
/// the way out; it has no meaning past this process pair.
pub(crate) fn serve(
    listener: UnixListener,
    path: String,
    table: std::sync::Arc<FdTable>,
    deadline: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("browser-fd-plane".into())
        .spawn(move || {
            let stream = accept_within(&listener, deadline);
            // Connected or not, the name has served its purpose: the peer holds the
            // connection, not the path.
            let _ = std::fs::remove_file(&path);
            let Some(stream) = stream else {
                debug!(target: "castaway::browser", "fd plane: browser never connected; staying on pidfd_getfd");
                return;
            };
            debug!(target: "castaway::browser", "fd plane: connected");
            loop {
                match recv_delivery(&stream) {
                    Ok(Some((id, fds))) => table.insert(id, fds),
                    Ok(None) => break, // clean EOF: the browser is gone
                    Err(e) => {
                        warn!(target: "castaway::browser", error = %e, "fd plane: receive failed");
                        break;
                    }
                }
            }
            debug!(target: "castaway::browser", "fd plane: closed");
        })
}

/// Wait for the one connection, without blocking forever on a peer that has no addon.
fn accept_within(listener: &UnixListener, deadline: Duration) -> Option<UnixStream> {
    if listener.set_nonblocking(true).is_err() {
        return None;
    }
    let give_up = Instant::now() + deadline;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    return None;
                }
                return Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return None,
        }
        if Instant::now() >= give_up {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Receive one delivery: 8 payload bytes and the fds that rode with them.
///
/// `Ok(None)` on a clean end of stream. The payload is read with `recvmsg` in a loop —
/// the sender writes it atomically, but assuming so would turn "cannot happen" into
/// framing corruption the day it does; ancillary data from any read in the loop is
/// collected, and the kernel's barrier semantics (reads never cross a control-message
/// boundary) keep one message's rights from bleeding into the next.
fn recv_delivery(stream: &UnixStream) -> std::io::Result<Option<(u64, Vec<OwnedFd>)>> {
    let mut header = [0u8; 8];
    let mut got = 0usize;
    let mut fds: Vec<OwnedFd> = Vec::new();
    while got < header.len() {
        let (n, mut newly) = recv_chunk(stream, &mut header[got..])?;
        fds.append(&mut newly);
        if n == 0 {
            if got == 0 && fds.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "fd plane closed mid-message",
            ));
        }
        got += n;
    }
    Ok(Some((u64::from_le_bytes(header), fds)))
}

/// One `recvmsg` with control-message space: how many payload bytes landed, and any
/// descriptors that came attached.
fn recv_chunk(stream: &UnixStream, buf: &mut [u8]) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: buf.len(),
    };
    // Aligned like a cmsghdr (u64-aligned on every Linux target).
    //
    // SAFETY (const): CMSG_SPACE is pure arithmetic on the length.
    #[allow(clippy::cast_possible_truncation)]
    const SPACE: usize =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<i32>()) as u32) } as usize;
    let mut control = [0u64; SPACE.div_ceil(8)];

    // SAFETY: msghdr is plain-old-data; zero then fill.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = SPACE;

    let received = loop {
        // SAFETY: the socket fd is live for the borrow of `stream`; msg points at the
        // locals above, all of which outlive the call. MSG_CMSG_CLOEXEC keeps received
        // descriptors from leaking into any child we spawn concurrently.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break usize::try_from(n).unwrap_or(0);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    };

    let mut fds = Vec::new();
    // SAFETY: msg's control fields were filled in by recvmsg; CMSG_FIRSTHDR/NXTHDR walk
    // only within msg_controllen, which the kernel set to what it wrote.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    while !cmsg.is_null() {
        // SAFETY: cmsg is non-null and inside the kernel-written control buffer.
        let (level, kind, len) =
            unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
        if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
            // SAFETY (const): CMSG_LEN(0) is the header size, pure arithmetic.
            let header_len = unsafe { libc::CMSG_LEN(0) } as usize;
            let count = len.saturating_sub(header_len) / std::mem::size_of::<RawFd>();
            for index in 0..count {
                // SAFETY: CMSG_DATA points at `count` RawFds the kernel wrote; each is
                // a fresh descriptor this process now owns exactly once.
                let fd = unsafe {
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(index)))
                };
                fds.push(fd);
            }
        }
        // SAFETY: msg/cmsg are the pair the kernel filled; NXTHDR returns null at the end.
        cmsg = unsafe { libc::CMSG_NXTHDR(&raw const msg, cmsg) };
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // More descriptors than MAX_FDS: the kernel closed the overflow, and what we
        // did receive cannot be trusted to be complete. Dropping ours closes them.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fd plane: control message truncated (more than MAX_FDS descriptors?)",
        ));
    }
    Ok((received, fds))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;

    /// The sender, mirrored in Rust: byte-for-byte what `castaway-browser-fd`'s
    /// `sendFds` emits, so the receive path is testable without an Electron.
    fn send_rights(stream: &UnixStream, id: u64, fds: &[RawFd]) {
        let payload = id.to_le_bytes();
        let mut iov = libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: payload.len(),
        };
        #[allow(clippy::cast_possible_truncation)]
        const SPACE: usize =
            unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<i32>()) as u32) } as usize;
        let mut control = [0u64; SPACE.div_ceil(8)];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast::<libc::c_void>();
        let rights_len = std::mem::size_of_val(fds);
        #[allow(clippy::cast_possible_truncation)]
        {
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(rights_len as u32) } as usize;
        }
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            #[allow(clippy::cast_possible_truncation)]
            {
                (*cmsg).cmsg_len = libc::CMSG_LEN(rights_len as u32) as usize;
            }
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                rights_len,
            );
        }
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &raw const msg, 0) };
        assert_eq!(sent, 8, "{}", std::io::Error::last_os_error());
    }

    fn dev_null() -> OwnedFd {
        OwnedFd::from(std::fs::File::open("/dev/null").unwrap())
    }

    #[test]
    fn a_delivery_round_trips_with_its_descriptors() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let file = dev_null();
        send_rights(&theirs, 42, &[file.as_raw_fd()]);
        let (id, fds) = recv_delivery(&ours).unwrap().expect("a delivery");
        assert_eq!(id, 42);
        assert_eq!(fds.len(), 1);
        // A genuinely new descriptor, usable in this process, independent of the
        // original — closing ours must not close theirs.
        assert_ne!(fds[0].as_raw_fd(), file.as_raw_fd());
        drop(fds);
        assert!(std::fs::File::from(file).metadata().is_ok());
    }

    #[test]
    fn two_deliveries_do_not_bleed_into_each_other() {
        // The property the whole framing rests on: SCM_RIGHTS is a read barrier, so
        // even two back-to-back sendmsgs come out as two deliveries with the right
        // descriptors attached to the right ids.
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (a, b) = (dev_null(), dev_null());
        send_rights(&theirs, 1, &[a.as_raw_fd()]);
        send_rights(&theirs, 2, &[b.as_raw_fd(), a.as_raw_fd()]);
        let (id1, fds1) = recv_delivery(&ours).unwrap().unwrap();
        let (id2, fds2) = recv_delivery(&ours).unwrap().unwrap();
        assert_eq!((id1, fds1.len()), (1, 1));
        assert_eq!((id2, fds2.len()), (2, 2));
    }

    #[test]
    fn a_closed_peer_is_a_clean_end_not_an_error() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(recv_delivery(&ours).unwrap().is_none());
    }

    #[test]
    fn a_peer_speaking_bytes_without_rights_is_an_eof_or_a_delivery_never_a_panic() {
        // Whatever connects to the socket: 8 bytes with no rights is a delivery with
        // no descriptors (the paint path then finds none and drops the frame); fewer
        // is a mid-message EOF.
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.write_all(&[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let (id, fds) = recv_delivery(&ours).unwrap().unwrap();
        assert_eq!((id, fds.len()), (1, 0));
        theirs.write_all(&[9, 9, 9]).unwrap();
        drop(theirs);
        assert!(recv_delivery(&ours).is_err());
    }

    #[test]
    fn the_table_pairs_ids_and_waits_out_a_late_delivery() {
        let table = Arc::new(FdTable::default());
        // Missing id: the deadline expires and reports the miss.
        let started = Instant::now();
        assert!(table.take(7, Duration::from_millis(50)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
        // Late delivery: a waiter parked on the condvar is woken by the insert.
        let waiter = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || table.take(8, Duration::from_secs(5)))
        };
        table.insert(8, vec![dev_null()]);
        let got = waiter.join().unwrap();
        assert_eq!(got.map(|fds| fds.len()), Some(1));
    }

    #[test]
    fn unclaimed_deliveries_are_bounded_oldest_first() {
        // Each entry holds live descriptors, so a browser that sends fds and then
        // never says `paint` must not be able to grow this without limit.
        let table = FdTable::default();
        for id in 0..(MAX_UNCLAIMED as u64 + 5) {
            table.insert(id, vec![dev_null()]);
        }
        assert_eq!(table.unclaimed(), MAX_UNCLAIMED);
        assert!(table.take(0, Duration::ZERO).is_none(), "oldest went first");
        assert!(table
            .take(MAX_UNCLAIMED as u64 + 4, Duration::ZERO)
            .is_some());
    }

    #[test]
    fn the_listener_accepts_the_host_app_and_serves_the_table() {
        // The whole thread, against a real socket pair: bind, connect the way the
        // addon does, deliver, claim.
        let dir = std::env::temp_dir().join(format!("castaway-fdplane-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("ctl").to_string_lossy().into_owned();
        let (path, listener) = bind(&base).unwrap();
        let table = Arc::new(FdTable::default());
        let thread = serve(
            listener,
            path.clone(),
            Arc::clone(&table),
            Duration::from_secs(5),
        )
        .unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let file = dev_null();
        send_rights(&client, 99, &[file.as_raw_fd()]);
        let fds = table
            .take(99, Duration::from_secs(5))
            .expect("the delivery reaches the table");
        assert_eq!(fds.len(), 1);
        drop(client);
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
