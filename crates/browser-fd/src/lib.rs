//! The browser host's fd sender: `SCM_RIGHTS` from JavaScript (#271).
//!
//! The production transport for the browser's painted frames is fd-passing over a Unix
//! socket — the `pidfd_getfd` route in `pipeline::hwaccel::remote_handle` depends on
//! ptrace policy (`kernel.yama.ptrace_scope`) and on the browser staying our direct
//! child, and a hardened box withdraws both. Passing the descriptor itself depends on
//! nothing but the socket. The one thing in the way is that **Node cannot say
//! `sendmsg(2)`**: no Electron or Node API attaches ancillary data to a socket write.
//! This crate is that missing call, packaged as the smallest native module that can
//! carry it.
//!
//! Two exports, loaded by `browser-host/main.js` via `process.dlopen`:
//!
//! - `connect(path)` — open and connect a blocking `AF_UNIX` stream socket, returning
//!   the raw fd. The addon owns the connect because Node's own `net.connect` hides its
//!   fd behind libuv internals, and reaching into `socket._handle.fd` would couple this
//!   to an undocumented layout.
//! - `sendFds(sock, id, fds)` — one `sendmsg` whose payload is the 8-byte little-endian
//!   `id` and whose ancillary data is the fds. The 8 bytes are the pairing key: the
//!   receiver (`pipeline::electron_fd_plane`) matches them against the paint message
//!   that follows on the control socket.
//!
//! Hand-rolled N-API rather than the napi crates, deliberately: the module is two
//! functions over integers, N-API is a frozen C ABI whose symbols the Electron binary
//! itself exports, and five `extern "C"` declarations weigh less than the napi-rs tree.
//! The cdylib links with the symbols unresolved and the dynamic loader binds them from
//! the host process at `dlopen` time — the standard N-API arrangement on Linux.
//!
//! Everything below `ADDON_SONAME` is `cfg(target_os = "linux")`: Windows pulls handles
//! with `DuplicateHandle` and needs none of this, and no other platform is a target.

// FFI crate (ground rule 8): the N-API boundary and sendmsg are why it exists. Every
// unsafe block carries its SAFETY comment.
#![allow(unsafe_code)]

/// What the built artifact is called, for the spawner that hands its path to the
/// browser host (`CASTAWAY_BROWSER_FD_ADDON`). One constant so the crate name and the
/// file the loader looks for cannot drift apart.
pub const ADDON_SONAME: &str = "libcastaway_browser_fd.so";

#[cfg(target_os = "linux")]
mod napi {
    use std::os::raw::{c_char, c_void};

    /// Opaque N-API handles. Pointers whose pointees are the engine's business.
    pub type Env = *mut c_void;
    pub type Value = *mut c_void;
    pub type CallbackInfo = *mut c_void;
    /// `napi_status`; 0 is `napi_ok`.
    pub type Status = i32;
    pub const OK: Status = 0;

    pub type Callback = unsafe extern "C" fn(Env, CallbackInfo) -> Value;

    // The five calls this module needs, out of the frozen `node_api.h` ABI. Resolved
    // from the host binary (Electron exports them) when the loader binds the cdylib.
    extern "C" {
        pub fn napi_create_function(
            env: Env,
            utf8name: *const c_char,
            length: usize,
            cb: Callback,
            data: *mut c_void,
            result: *mut Value,
        ) -> Status;
        pub fn napi_set_named_property(
            env: Env,
            object: Value,
            utf8name: *const c_char,
            value: Value,
        ) -> Status;
        pub fn napi_get_cb_info(
            env: Env,
            cbinfo: CallbackInfo,
            argc: *mut usize,
            argv: *mut Value,
            this_arg: *mut Value,
            data: *mut *mut c_void,
        ) -> Status;
        pub fn napi_get_value_string_utf8(
            env: Env,
            value: Value,
            buf: *mut c_char,
            bufsize: usize,
            result: *mut usize,
        ) -> Status;
        pub fn napi_get_value_int64(env: Env, value: Value, result: *mut i64) -> Status;
        pub fn napi_get_array_length(env: Env, value: Value, result: *mut u32) -> Status;
        pub fn napi_get_element(env: Env, object: Value, index: u32, result: *mut Value) -> Status;
        pub fn napi_create_int32(env: Env, value: i32, result: *mut Value) -> Status;
        pub fn napi_get_undefined(env: Env, result: *mut Value) -> Status;
        pub fn napi_throw_error(env: Env, code: *const c_char, msg: *const c_char) -> Status;
    }
}

#[cfg(target_os = "linux")]
mod addon {
    use std::os::raw::c_char;

    use super::napi;

    /// The most fds one message may carry. Four covers every plane layout Chromium
    /// produces; the receiver sizes its control-message buffer to the same number.
    pub const MAX_FDS: usize = 4;

    /// Throw a JS error and return the value a throwing callback must return.
    ///
    /// `msg` must be NUL-terminated (every caller passes a literal with `\0`).
    fn throw(env: napi::Env, msg: &str) -> napi::Value {
        debug_assert!(msg.ends_with('\0'));
        // SAFETY: `env` is the live environment N-API handed the callback; the message
        // is a NUL-terminated literal; a null code pointer is documented as "no code".
        unsafe {
            napi::napi_throw_error(env, std::ptr::null(), msg.as_ptr().cast::<c_char>());
        }
        std::ptr::null_mut()
    }

    /// `connect(path: string) -> fd: number`
    ///
    /// Blocking, which is what the caller wants: the browser host connects once at
    /// startup, before any frame exists to send, and a socket that is not there yet is
    /// an error rather than something to wait on.
    pub unsafe extern "C" fn connect(env: napi::Env, info: napi::CallbackInfo) -> napi::Value {
        let mut argv: [napi::Value; 1] = [std::ptr::null_mut()];
        let mut argc = argv.len();
        // SAFETY: env/info are the live callback pair; argv is sized by argc.
        let status = unsafe {
            napi::napi_get_cb_info(
                env,
                info,
                &raw mut argc,
                argv.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != napi::OK || argc < 1 {
            return throw(env, "connect(path) takes a socket path\0");
        }

        let mut path = [0u8; 512];
        let mut len = 0usize;
        // SAFETY: argv[0] came from napi_get_cb_info above; the buffer and its size
        // agree; N-API NUL-terminates and reports the copied length.
        let status = unsafe {
            napi::napi_get_value_string_utf8(
                env,
                argv[0],
                path.as_mut_ptr().cast::<c_char>(),
                path.len(),
                &raw mut len,
            )
        };
        if status != napi::OK || len == 0 || len >= path.len() - 1 {
            return throw(env, "connect(path): not a usable path string\0");
        }

        // SAFETY: plain socket(2); no memory crosses the boundary.
        let sock =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if sock < 0 {
            return throw(env, "connect: socket(AF_UNIX) failed\0");
        }

        let mut addr: libc::sockaddr_un =
            // SAFETY: sockaddr_un is plain-old-data; all-zeroes is a valid initial state.
            unsafe { std::mem::zeroed() };
        addr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).unwrap_or(1);
        if len >= addr.sun_path.len() {
            // SAFETY: sock is the fd we just opened.
            unsafe { libc::close(sock) };
            return throw(env, "connect: path too long for sockaddr_un\0");
        }
        for (dst, src) in addr.sun_path.iter_mut().zip(&path[..len]) {
            *dst = src.cast_signed();
        }
        #[allow(clippy::cast_possible_truncation)]
        let addr_len = (std::mem::size_of::<libc::sa_family_t>() + len + 1) as libc::socklen_t;
        // SAFETY: sock is our fd; addr is a filled sockaddr_un whose length addr_len
        // covers the family, the path bytes and the terminating NUL.
        let rc = unsafe {
            libc::connect(
                sock,
                std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
                addr_len,
            )
        };
        if rc != 0 {
            // SAFETY: sock is the fd we opened above.
            unsafe { libc::close(sock) };
            return throw(env, "connect: connect(2) to the fd socket failed\0");
        }

        let mut out: napi::Value = std::ptr::null_mut();
        // SAFETY: env is live; out receives the created number.
        let status = unsafe { napi::napi_create_int32(env, sock, &raw mut out) };
        if status != napi::OK {
            // SAFETY: sock is the fd we opened above; JS never learned it.
            unsafe { libc::close(sock) };
            return throw(env, "connect: could not box the fd\0");
        }
        out
    }

    /// `sendFds(sock: number, id: number, fds: number[])`
    ///
    /// One `sendmsg`: the 8-byte little-endian `id` as payload, the fds as
    /// `SCM_RIGHTS`. The payload rides with the rights on purpose — ancillary data is
    /// delivered attached to its bytes, so the receiver can never mispair an id with
    /// another message's descriptors.
    pub unsafe extern "C" fn send_fds(env: napi::Env, info: napi::CallbackInfo) -> napi::Value {
        let mut argv: [napi::Value; 3] = [std::ptr::null_mut(); 3];
        let mut argc = argv.len();
        // SAFETY: env/info are the live callback pair; argv is sized by argc.
        let status = unsafe {
            napi::napi_get_cb_info(
                env,
                info,
                &raw mut argc,
                argv.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != napi::OK || argc < 3 {
            return throw(env, "sendFds(sock, id, fds) takes three arguments\0");
        }

        let mut sock: i64 = -1;
        let mut id: i64 = 0;
        // SAFETY: argv[0]/argv[1] came from napi_get_cb_info; the out-pointers are live.
        let ok = unsafe {
            napi::napi_get_value_int64(env, argv[0], &raw mut sock) == napi::OK
                && napi::napi_get_value_int64(env, argv[1], &raw mut id) == napi::OK
        };
        let Ok(sock) = i32::try_from(sock) else {
            return throw(env, "sendFds: socket is not an fd\0");
        };
        if !ok || sock < 0 {
            return throw(env, "sendFds: socket and id must be numbers\0");
        }

        let mut count: u32 = 0;
        // SAFETY: argv[2] came from napi_get_cb_info; count is a live out-pointer.
        if unsafe { napi::napi_get_array_length(env, argv[2], &raw mut count) } != napi::OK {
            return throw(env, "sendFds: fds must be an array\0");
        }
        if count == 0 || count as usize > MAX_FDS {
            return throw(env, "sendFds: between 1 and 4 fds\0");
        }
        let mut fds = [0i32; MAX_FDS];
        for index in 0..count {
            let mut element: napi::Value = std::ptr::null_mut();
            let mut fd: i64 = -1;
            // SAFETY: argv[2] is the array checked above; index < its length; the
            // out-pointers are live locals.
            let ok = unsafe {
                napi::napi_get_element(env, argv[2], index, &raw mut element) == napi::OK
                    && napi::napi_get_value_int64(env, element, &raw mut fd) == napi::OK
            };
            let Ok(fd) = i32::try_from(fd) else {
                return throw(env, "sendFds: fd out of range\0");
            };
            if !ok || fd < 0 {
                return throw(env, "sendFds: fds must be non-negative numbers\0");
            }
            fds[index as usize] = fd;
        }

        if send_rights(sock, id.cast_unsigned(), &fds[..count as usize]).is_err() {
            return throw(env, "sendFds: sendmsg(SCM_RIGHTS) failed\0");
        }

        let mut out: napi::Value = std::ptr::null_mut();
        // SAFETY: env is live; out receives undefined.
        unsafe { napi::napi_get_undefined(env, &raw mut out) };
        out
    }

    /// The `sendmsg` itself: 8 payload bytes, `SCM_RIGHTS` ancillary, `MSG_NOSIGNAL`
    /// so a receiver that died turns into an error rather than a `SIGPIPE`.
    fn send_rights(sock: i32, id: u64, fds: &[i32]) -> Result<(), ()> {
        let payload = id.to_le_bytes();
        let mut iov = libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: payload.len(),
        };

        // The control buffer, aligned like a cmsghdr (u64-aligned on every Linux
        // target) so CMSG_FIRSTHDR's pointer arithmetic is sound.
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
        let rights_len = std::mem::size_of_val(fds);
        #[allow(clippy::cast_possible_truncation)]
        // SAFETY: CMSG_SPACE is pure arithmetic on the length.
        let control_len = unsafe { libc::CMSG_SPACE(rights_len as u32) } as usize;
        msg.msg_controllen = control_len;

        // SAFETY: msg_control/msg_controllen were just set to a buffer that satisfies
        // cmsg alignment; CMSG_FIRSTHDR only does pointer arithmetic on them.
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
        if cmsg.is_null() {
            return Err(());
        }
        // SAFETY: cmsg points into `control`, which is live and writable.
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

        loop {
            // SAFETY: sock is a caller-supplied fd (a bad one fails with EBADF); msg
            // points at the locals built above, all of which outlive the call.
            let sent = unsafe { libc::sendmsg(sock, &raw const msg, libc::MSG_NOSIGNAL) };
            if sent >= 0 {
                return Ok(());
            }
            // SAFETY: errno read; no memory crosses.
            let errno = unsafe { *libc::__errno_location() };
            if errno != libc::EINTR {
                return Err(());
            }
        }
    }
}

/// The module registration N-API looks for by name in the loaded library.
///
/// # Safety
/// Called by the N-API loader with a live `env` and `exports` object; everything it does
/// is through checked N-API calls on that pair.
#[cfg(target_os = "linux")]
#[no_mangle]
pub unsafe extern "C" fn napi_register_module_v1(
    env: napi::Env,
    exports: napi::Value,
) -> napi::Value {
    let functions: [(&std::ffi::CStr, napi::Callback); 2] =
        [(c"connect", addon::connect), (c"sendFds", addon::send_fds)];
    for (name, callback) in functions {
        let mut function: napi::Value = std::ptr::null_mut();
        // SAFETY: env is the loader's live environment; the name is NUL-terminated;
        // `function` receives the created value.
        let status = unsafe {
            napi::napi_create_function(
                env,
                name.as_ptr(),
                name.to_bytes().len(),
                callback,
                std::ptr::null_mut(),
                &raw mut function,
            )
        };
        if status != napi::OK {
            return exports;
        }
        // SAFETY: exports is the loader's live exports object; function was just made.
        unsafe { napi::napi_set_named_property(env, exports, name.as_ptr(), function) };
    }
    exports
}
