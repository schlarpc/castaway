//! One lock over "open a device on the GPU".
//!
//! This process opens the same graphics hardware through three unrelated APIs: wgpu's
//! Vulkan (or DX12) device for the compositor, libavutil's VA-API (or D3D11VA) device for
//! the hardware *decoder*, and another for the stream's hardware *encoder*. Each brings up
//! its own driver stack, and on Mesa they are the same shared objects — `radeonsi` behind
//! VA-API, `radv` behind Vulkan — initialising concurrently in one address space.
//!
//! That is not safe. Opening a VA-API device while a wgpu device is being created
//! segfaults about one time in six on the development box, inside the driver, with no
//! Rust frame in the stack. It was found by the test suite doing exactly that: the stream
//! encoder's tests and the NV12 conversion's tests run in parallel by default.
//!
//! It is not only a test artefact. A cast starting at the same moment as the output stream
//! puts a decode device and an encode device in the same race, on an unattended panel,
//! where the symptom is the process disappearing.
//!
//! So: every device open takes this lock. It costs nothing — opens happen a handful of
//! times in a process's life, and none of them is on a per-frame path — and what it buys
//! is that the class of crash cannot happen at all rather than being rare enough to blame
//! on the driver.

use std::sync::{Mutex, MutexGuard, OnceLock};

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Hold this for the duration of a device open.
///
/// A poisoned lock is recovered rather than propagated: the invariant being protected is
/// the *driver's*, not ours, and a previous opener that panicked holding this has left
/// nothing of ours inconsistent. Refusing to open a device ever again because one did
/// would turn a survivable failure into a permanent one.
#[must_use]
pub fn opening_device() -> MutexGuard<'static, ()> {
    lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_one_lock_for_the_whole_process() {
        // Two guards from different callers must be the same lock, or the serialisation
        // this exists for does not happen. Taken on two threads because taking it twice
        // on one would simply deadlock.
        let held = opening_device();
        let grabbed = std::thread::spawn(|| {
            let _second = opening_device();
            true
        });
        assert!(!grabbed.is_finished(), "the second open should be waiting");
        drop(held);
        assert!(grabbed.join().unwrap_or(false));
    }

    #[test]
    fn a_panicking_opener_does_not_close_the_device_for_everyone_else() {
        let _ = std::thread::spawn(|| {
            let _guard = opening_device();
            panic!("an opener that fell over");
        })
        .join();
        // The lock is poisoned now. Opening a device must still be possible: what it
        // guards is the driver's own initialisation, and nothing of ours was left half
        // written.
        let _guard = opening_device();
    }
}
