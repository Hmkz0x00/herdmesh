//! Owner-only secret files: node key and room secrets (ADR-002 §5).
//!
//! - [`create_private`] creates a new file only its owner can access: mode
//!   0600 on Unix; on Windows a protected (non-inherited) DACL with a single
//!   full-access ACE for the current user, applied atomically at creation.
//! - [`check_private`] reports whether an existing file is broader than that:
//!   Unix group/other mode bits; Windows any allow-ACE for a SID other than
//!   the current user, `SYSTEM` or `BUILTIN\Administrators` (those two can
//!   bypass a DACL anyway), or a null DACL.
//! - [`read_private`] reads a secret file and refuses (with a warning) when
//!   it is insecure, unless the caller passed `allow_insecure`.

use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SecureFileError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "{path} is readable by others ({reason}); fix its permissions or pass --allow-insecure-key"
    )]
    Insecure { path: PathBuf, reason: String },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SecureFileError + '_ {
    move |source| SecureFileError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Create `path` (must not exist) with owner-only access, opened for writing.
pub fn create_private(path: &Path) -> Result<File, SecureFileError> {
    sys::create_private(path).map_err(io_err(path))
}

/// `Ok(None)` if only the owner can access `path`, else why not.
pub fn check_private(path: &Path) -> Result<Option<String>, SecureFileError> {
    sys::check_private(path).map_err(io_err(path))
}

/// Read a secret file, refusing one others can access unless
/// `allow_insecure` (then only warn).
pub fn read_private(path: &Path, allow_insecure: bool) -> Result<Vec<u8>, SecureFileError> {
    let bytes = std::fs::read(path).map_err(io_err(path))?;
    if let Some(reason) = check_private(path)? {
        tracing::warn!(path = %path.display(), %reason, "secret file is not owner-only");
        if !allow_insecure {
            return Err(SecureFileError::Insecure {
                path: path.to_owned(),
                reason,
            });
        }
    }
    Ok(bytes)
}

#[cfg(unix)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    use std::path::Path;

    pub fn create_private(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    pub fn check_private(path: &Path) -> std::io::Result<Option<String>> {
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        Ok((mode & 0o077 != 0).then(|| format!("mode {mode:04o}, expected 0600")))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod sys {
    //! Win32 FFI (windows-sys 0.61). Every pointer handed out by the OS is
    //! released by a guard; SIDs are copied into owned buffers.

    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{FromRawHandle as _, RawHandle};
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CopySid, CreateWellKnownSid,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetTokenInformation, PSID,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    /// SECURITY_MAX_SID_SIZE.
    const MAX_SID: usize = 68;

    /// Frees a `LocalAlloc`ed pointer on drop.
    struct Local(*mut core::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from an API documented to return
                // LocalAlloc memory, and is freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a valid handle we opened, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// An owned SID; `u64` storage keeps it suitably aligned.
    struct Sid(Vec<u64>);
    impl Sid {
        fn ptr(&self) -> PSID {
            self.0.as_ptr().cast_mut().cast()
        }

        fn copy_from(sid: PSID) -> io::Result<Self> {
            // SAFETY: `sid` is a valid SID owned by the caller's buffer.
            let len = unsafe { GetLengthSid(sid) };
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            // SAFETY: `buf` holds at least `len` bytes.
            check(unsafe { CopySid(len, buf.as_mut_ptr().cast(), sid) })?;
            Ok(Self(buf))
        }

        fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Self> {
            let mut buf = vec![0u64; MAX_SID.div_ceil(8)];
            #[allow(clippy::cast_possible_truncation)]
            let mut len = (buf.len() * 8) as u32;
            // SAFETY: buffer of `len` bytes; no domain SID needed for these.
            check(unsafe {
                CreateWellKnownSid(kind, null_mut(), buf.as_mut_ptr().cast(), &raw mut len)
            })?;
            Ok(Self(buf))
        }

        fn current_user() -> io::Result<Self> {
            let mut token: HANDLE = null_mut();
            // SAFETY: pseudo-handle of this process; out-param is ours.
            check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) })?;
            let token = Handle(token);
            let mut len = 0u32;
            // SAFETY: size query with a null buffer; expected to fail with
            // ERROR_INSUFFICIENT_BUFFER and set `len`.
            unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &raw mut len) };
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            // SAFETY: buffer of at least `len` bytes, 8-aligned.
            check(unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    buf.as_mut_ptr().cast(),
                    len,
                    &raw mut len,
                )
            })?;
            // SAFETY: on success the buffer starts with a TOKEN_USER whose
            // SID points into the same buffer, still alive here.
            let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
            Self::copy_from(user.User.Sid)
        }

        fn eq(&self, other: PSID) -> bool {
            // SAFETY: both are valid SIDs.
            unsafe { EqualSid(self.ptr(), other) != 0 }
        }
    }

    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut out: windows_sys::core::PWSTR = null_mut();
        // SAFETY: valid SID; the API allocates `out` with LocalAlloc.
        check(unsafe { ConvertSidToStringSidW(sid, &raw mut out) })?;
        let _free = Local(out.cast());
        let mut n = 0;
        // SAFETY: `out` is a NUL-terminated wide string.
        while unsafe { *out.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: `n` u16s were just read from `out`.
        let s = unsafe { std::slice::from_raw_parts(out, n) };
        Ok(String::from_utf16_lossy(s))
    }

    pub fn create_private(path: &Path) -> io::Result<File> {
        let user = Sid::current_user()?;
        // Protected DACL (P: no inheritance), one ACE: full access (FA) for us.
        let sddl = format!("D:P(A;;FA;;;{})", sid_string(user.ptr())?);
        let sddl = wide(std::ffi::OsStr::new(&sddl));
        let mut sd = null_mut();
        // SAFETY: NUL-terminated SDDL; `sd` receives a LocalAlloc pointer.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &raw mut sd,
                null_mut(),
            )
        })?;
        let sd = Local(sd);
        #[allow(clippy::cast_possible_truncation)]
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let wpath = wide(path.as_os_str());
        // SAFETY: valid path and security attributes for the call's duration.
        let h = unsafe {
            CreateFileW(
                wpath.as_ptr(),
                GENERIC_WRITE,
                0,
                &raw const sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh, owned file handle; File takes ownership.
        Ok(unsafe { File::from_raw_handle(h as RawHandle) })
    }

    pub fn check_private(path: &Path) -> io::Result<Option<String>> {
        let allowed = [
            Sid::current_user()?,
            Sid::well_known(WinLocalSystemSid)?,
            Sid::well_known(WinBuiltinAdministratorsSid)?,
        ];
        let wpath = wide(path.as_os_str());
        let mut dacl: *mut ACL = null_mut();
        let mut sd = null_mut();
        // SAFETY: out-params are ours; `sd` is LocalAlloc'ed and owns `dacl`.
        let err = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &raw mut dacl,
                null_mut(),
                &raw mut sd,
            )
        };
        let _sd = Local(sd);
        if err != 0 {
            #[allow(clippy::cast_possible_wrap)]
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        if dacl.is_null() {
            return Ok(Some("no DACL: everyone has access".into()));
        }
        // SAFETY: non-null DACL inside the live descriptor.
        let count = unsafe { (*dacl).AceCount };
        let mut others = Vec::new();
        for i in 0..u32::from(count) {
            let mut ace = null_mut();
            // SAFETY: index below AceCount.
            check(unsafe { GetAce(dacl, i, &raw mut ace) })?;
            // SAFETY: every ACE starts with an ACE_HEADER.
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            match header.AceType {
                ACCESS_ALLOWED_ACE_TYPE => {
                    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                    // SAFETY: the SID starts at SidStart inside this ACE.
                    let sid: PSID = unsafe { (&raw mut (*ace).SidStart).cast() };
                    if !allowed.iter().any(|a| a.eq(sid)) {
                        others.push(sid_string(sid)?);
                    }
                }
                ACCESS_DENIED_ACE_TYPE => {}
                t => others.push(format!("ACE type {t}")),
            }
        }
        Ok((!others.is_empty()).then(|| format!("access granted to {}", others.join(", "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn new_secret(dir: &tempfile::TempDir) -> PathBuf {
        let p = dir.path().join("secret.key");
        let mut f = create_private(&p).unwrap();
        f.write_all(&[7; 32]).unwrap();
        drop(f);
        p
    }

    #[test]
    fn created_file_is_owner_only_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let p = new_secret(&dir);
        assert_eq!(check_private(&p).unwrap(), None);
        assert_eq!(read_private(&p, false).unwrap(), vec![7; 32]);
    }

    #[test]
    fn create_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = new_secret(&dir);
        assert!(matches!(
            create_private(&p),
            Err(SecureFileError::Io { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), vec![7; 32]);
    }

    fn broaden(p: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        {
            // Grant Everyone (S-1-1-0) read access.
            let st = std::process::Command::new("icacls")
                .arg(p)
                .args(["/grant", "*S-1-1-0:R"])
                .output()
                .unwrap();
            assert!(st.status.success(), "{st:?}");
        }
    }

    #[test]
    fn broader_file_is_refused_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let p = new_secret(&dir);
        broaden(&p);
        let reason = check_private(&p).unwrap().expect("not flagged");
        #[cfg(unix)]
        assert!(reason.contains("0644"), "{reason}");
        #[cfg(windows)]
        assert!(reason.contains("S-1-1-0"), "{reason}");
        let err = read_private(&p, false).unwrap_err();
        assert!(matches!(err, SecureFileError::Insecure { .. }), "{err}");
        assert!(err.to_string().contains("--allow-insecure-key"));
        assert_eq!(read_private(&p, true).unwrap(), vec![7; 32]);
    }

    #[cfg(windows)]
    #[test]
    fn plain_std_file_inheriting_users_group_is_flagged() {
        // A file created with std inherits its directory's DACL. Granting
        // BUILTIN\Users (S-1-5-32-545) mimics a shared parent directory.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain.key");
        std::fs::write(&p, [1; 32]).unwrap();
        let st = std::process::Command::new("icacls")
            .arg(&p)
            .args(["/grant", "*S-1-5-32-545:R"])
            .output()
            .unwrap();
        assert!(st.status.success());
        let reason = check_private(&p).unwrap().expect("not flagged");
        assert!(reason.contains("S-1-5-32-545"), "{reason}");
    }
}
