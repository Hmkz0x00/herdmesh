//! Who runs the other end of a local socket (GATE-2 N1).
//!
//! The client checks this *before* it writes a byte: a listener squatting
//! the per-user name under another account is refused.
//! - Windows: pipe server pid → process token → user SID (text form).
//! - Unix: the socket lives in a 0700 directory we own (checked), and the
//!   peer's effective uid must be ours where the OS reports it.

use std::io;

use interprocess::local_socket::tokio::{Stream, prelude::*};

/// An OS user identity: SID text on Windows, uid on Unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerId(String);

impl OwnerId {
    /// The user running this process.
    pub fn current() -> io::Result<Self> {
        #[cfg(windows)]
        {
            hm_net::secure_file::current_user_sid().map(Self)
        }
        #[cfg(unix)]
        {
            // Creates/verifies the private socket dir and proves our uid.
            crate::ipc::ensure_private_dir(&crate::ipc::socket_dir()).map(|u| Self(u.to_string()))
        }
    }

    /// An arbitrary identity, for tests that simulate a foreign server.
    #[doc(hidden)]
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// The identity of the process serving `s` (client side).
pub fn server_owner(s: &Stream) -> io::Result<OwnerId> {
    #[cfg(windows)]
    {
        let pid = s
            .peer_creds()?
            .pid()
            .ok_or_else(|| io::Error::other("pipe server pid unavailable"))?;
        sys::process_user_sid(pid).map(OwnerId)
    }
    #[cfg(unix)]
    {
        // Reaching the socket at all required access to our 0700 dir;
        // where the OS reports the peer uid, it must match too.
        let me = OwnerId::current()?;
        match s.peer_creds()?.euid() {
            Some(uid) => Ok(OwnerId(uid.to_string())),
            None => Ok(me),
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod sys {
    //! Win32 FFI (windows-sys 0.61). Handles and OS allocations are released
    //! by guards.

    use std::io;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a valid handle we opened, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    struct Local(*mut core::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: LocalAlloc'ed by the API that returned it; freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// User SID (text) of process `pid`. Fails if the process belongs to a
    /// user whose token we may not query: callers treat that as foreign.
    pub fn process_user_sid(pid: u32) -> io::Result<String> {
        // SAFETY: plain call; a null return is the failure case.
        let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if p.is_null() {
            return Err(io::Error::last_os_error());
        }
        let p = Handle(p);
        let mut token: HANDLE = null_mut();
        // SAFETY: valid process handle; out-param is ours.
        check(unsafe { OpenProcessToken(p.0, TOKEN_QUERY, &raw mut token) })?;
        let token = Handle(token);
        let mut len = 0u32;
        // SAFETY: size query with a null buffer; sets `len`.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &raw mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: 8-aligned buffer of at least `len` bytes.
        check(unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                len,
                &raw mut len,
            )
        })?;
        // SAFETY: on success the buffer starts with a TOKEN_USER whose SID
        // points into the same buffer, alive for this scope.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let mut out: windows_sys::core::PWSTR = null_mut();
        // SAFETY: valid SID; the API LocalAllocs `out`.
        check(unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut out) })?;
        let _free = Local(out.cast());
        let mut n = 0;
        // SAFETY: `out` is NUL-terminated.
        while unsafe { *out.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: `n` u16s were just read from `out`.
        let s = unsafe { std::slice::from_raw_parts(out, n) };
        Ok(String::from_utf16_lossy(s))
    }
}
