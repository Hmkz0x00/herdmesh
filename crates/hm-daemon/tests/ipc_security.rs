//! Same-user enforcement on the IPC endpoint.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ed25519_dalek::SigningKey;
use hm_daemon::ipc::{Server, Token};
use hm_daemon::{Daemon, NullNet};

static N: AtomicUsize = AtomicUsize::new(0);

fn name() -> String {
    format!(
        "herdmesh-sec-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn daemon() -> Arc<Daemon> {
    Arc::new(Daemon::new(
        SigningKey::from_bytes(&[8; 32]),
        Arc::new(NullNet),
    ))
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;

    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions};

    use super::*;

    fn pipe(name: &str) -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\{name}"))
    }

    /// Control: a pipe open to Everyone is never judged owner-only, so the DACL check below is not vacuous.
    #[tokio::test]
    async fn control_everyone_pipe_is_not_owner_only() {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let n = name();
        let sddl = widestring::U16CString::from_str("D:(A;;GA;;;WD)").unwrap();
        let _l = ListenerOptions::new()
            .name(n.clone().to_ns_name::<GenericNamespaced>().unwrap())
            .security_descriptor(SecurityDescriptor::deserialize(&sddl).unwrap())
            .create_tokio()
            .unwrap();
        // The checker either reports the extra grant or cannot evaluate the
        // ACL (it errors on this pipe); it must never say "owner-only".
        let r = hm_net::secure_file::check_private(&pipe(&n));
        assert!(!matches!(r, Ok(None)), "Everyone pipe judged owner-only");
    }

    #[tokio::test]
    async fn daemon_pipe_dacl_is_owner_only() {
        let n = name();
        let _srv = Server::bind(&n, Token::from_bytes([1; 32]))
            .unwrap()
            .spawn(daemon());
        let r = hm_net::secure_file::check_private(&pipe(&n)).unwrap();
        assert_eq!(r, None, "pipe must grant only the current user");
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::PermissionsExt as _;

    use hm_daemon::ipc::{ensure_private_dir, socket_dir};

    use super::*;

    #[test]
    fn private_dir_is_created_0700_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("a").join("hm");
        let uid = ensure_private_dir(&d).unwrap();
        let mode = std::fs::metadata(&d).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(ensure_private_dir(&d).unwrap(), uid);
    }

    #[test]
    fn dir_open_to_others_or_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&open).is_err());
        let link = tmp.path().join("link");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }

    #[tokio::test]
    async fn socket_is_0600_in_a_0700_dir_and_serves_its_owner() {
        let n = name();
        let _srv = Server::bind(&n, Token::from_bytes([1; 32]))
            .unwrap()
            .spawn(daemon());
        let dir = socket_dir();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o077, 0);
        let sock = dir.join(format!("{n}.sock"));
        let sock_mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(sock_mode & 0o077, 0);
        assert!(hm_daemon::ipc::connect_raw(&n).await.is_ok());
    }
}

#[tokio::test]
async fn names_with_path_separators_are_rejected() {
    for bad in ["", "../x", "a/b", "a\\b"] {
        assert!(hm_daemon::ipc::connect_raw(bad).await.is_err(), "{bad:?}");
        assert!(
            Server::bind(bad, Token::from_bytes([1; 32])).is_err(),
            "{bad:?}"
        );
    }
}
