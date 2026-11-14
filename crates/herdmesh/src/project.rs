//! Which room a project folder belongs to.
//!
//! The binding is keyed by a stable identity of the folder (a hash of its
//! canonical absolute path) and stored in the data dir, never inside the
//! project: nothing in a (possibly untrusted) checkout can point setup or
//! the room commands at a different room, and two folders that merely share
//! a name get different rooms (GATE-4 N13, N14).
//!
//! `projects/<key>.toml` holds `room_id` (hex) and the path it was made for.
//! Room commands use the nearest bound folder at or above the current one.

use std::io;
use std::path::{Path, PathBuf};

const DIR: &str = "projects";
const DOMAIN: &str = "herdmesh-project-v2";

/// The folder's real path with links resolved: the project identity input.
pub fn canonical_root(p: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

/// The path's raw OS bytes, lossless: Unix bytes as they are, Windows UTF-16
/// code units (which may hold unpaired surrogates). A lossy `String` would give
/// two sibling folders whose names differ only in invalid bytes the same
/// identity (GATE-5 N18).
fn raw_bytes(p: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        p.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        p.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

fn path_id(p: &std::ffi::OsStr) -> String {
    let mut h = blake3::Hasher::new();
    h.update(DOMAIN.as_bytes());
    h.update(&[0]);
    h.update(&raw_bytes(p));
    h.finalize().to_hex().to_string()
}

/// Stable identity of a canonical folder path: 32 hex chars.
pub fn project_key(canonical: &Path) -> String {
    path_id(canonical.as_os_str())[..32].to_owned()
}

fn binding_file(data_root: &Path, key: &str) -> PathBuf {
    data_root.join(DIR).join(format!("{key}.toml"))
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    room_id: String,
    /// Full lossless path identity, checked on lookup.
    path_id: String,
    /// For humans only (lossy).
    path: String,
}

fn valid_room_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Bind `canonical` to the room `room_id` (64 hex chars). Replaces an
/// earlier binding of the same folder.
pub fn bind(data_root: &Path, canonical: &Path, room_id: &str) -> io::Result<()> {
    if !valid_room_id(room_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "room id must be 64 lowercase hex characters",
        ));
    }
    let file = binding_file(data_root, &project_key(canonical));
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string(&Binding {
        room_id: room_id.to_owned(),
        path_id: path_id(canonical.as_os_str()),
        path: canonical.display().to_string(),
    })
    .map_err(io::Error::other)?;
    let tmp = file.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &file)
}

/// The room id this exact folder is bound to.
pub fn bound_room(data_root: &Path, canonical: &Path) -> Option<String> {
    let text = std::fs::read_to_string(binding_file(data_root, &project_key(canonical))).ok()?;
    let b: Binding = toml::from_str(&text).ok()?;
    (valid_room_id(&b.room_id) && b.path_id == path_id(canonical.as_os_str())).then_some(b.room_id)
}

/// The room of `start` or the nearest parent folder that is bound.
pub fn find_room(data_root: &Path, start: &Path) -> Option<String> {
    let start = canonical_root(start).ok()?;
    start.ancestors().find_map(|dir| bound_room(data_root, dir))
}

/// An explicit room wins; otherwise the folder's bound room (as a room id,
/// which every room command accepts); otherwise an error saying how to fix it.
pub fn room_arg(data_root: &Path, given: Option<&str>, cwd: &Path) -> Result<String, String> {
    if let Some(g) = given {
        return Ok(g.to_owned());
    }
    find_room(data_root, cwd).ok_or_else(|| {
        "no room given and this folder is not bound to a herdmesh room; run `herdmesh setup` here or pass the room name"
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const R1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const R2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        let proj = t.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let proj = canonical_root(&proj).unwrap();
        (t, data, proj)
    }

    #[test]
    fn binds_by_path_identity_and_finds_it_from_subfolders() {
        let (_t, data, proj) = dirs();
        let sub = proj.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_room(&data, &sub), None);
        bind(&data, &proj, R1).unwrap();
        assert_eq!(bound_room(&data, &proj).as_deref(), Some(R1));
        assert_eq!(find_room(&data, &sub).as_deref(), Some(R1));
    }

    #[test]
    fn same_named_folders_have_distinct_identities() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        let a = t.path().join("one/app");
        let b = t.path().join("two/app");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (canonical_root(&a).unwrap(), canonical_root(&b).unwrap());
        assert_ne!(project_key(&a), project_key(&b));
        bind(&data, &a, R1).unwrap();
        assert_eq!(bound_room(&data, &b), None, "basename must not bind");
        bind(&data, &b, R2).unwrap();
        assert_eq!(bound_room(&data, &a).as_deref(), Some(R1));
        assert_eq!(bound_room(&data, &b).as_deref(), Some(R2));
        assert_eq!(project_key(&a), project_key(&a));
        assert_eq!(project_key(&a).len(), 32);
    }

    #[test]
    fn nearest_binding_wins_and_rebinding_replaces() {
        let (_t, data, proj) = dirs();
        let inner = proj.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let inner = canonical_root(&inner).unwrap();
        bind(&data, &proj, R1).unwrap();
        bind(&data, &inner, R2).unwrap();
        assert_eq!(find_room(&data, &inner).as_deref(), Some(R2));
        bind(&data, &inner, R1).unwrap();
        assert_eq!(find_room(&data, &inner).as_deref(), Some(R1));
    }

    #[test]
    fn bad_ids_and_corrupt_files_are_ignored() {
        let (_t, data, proj) = dirs();
        assert!(bind(&data, &proj, "../../evil").is_err());
        assert!(bind(&data, &proj, &"A".repeat(64)).is_err());
        bind(&data, &proj, R1).unwrap();
        std::fs::write(binding_file(&data, &project_key(&proj)), "junk = [").unwrap();
        assert_eq!(bound_room(&data, &proj), None);
        std::fs::write(
            binding_file(&data, &project_key(&proj)),
            "room_id = \"../x\"\npath_id = \"x\"\npath = \"p\"\n",
        )
        .unwrap();
        assert_eq!(bound_room(&data, &proj), None);
    }

    #[test]
    fn explicit_room_beats_binding_and_missing_is_a_helpful_error() {
        let (_t, data, proj) = dirs();
        assert_eq!(room_arg(&data, Some("x"), &proj).unwrap(), "x");
        let e = room_arg(&data, None, &proj).unwrap_err();
        assert!(e.contains("herdmesh setup"), "{e}");
        bind(&data, &proj, R1).unwrap();
        assert_eq!(room_arg(&data, None, &proj).unwrap(), R1);
        assert_eq!(room_arg(&data, Some("q"), &proj).unwrap(), "q");
    }

    #[test]
    fn a_checkout_cannot_bind_itself() {
        // Nothing inside the folder is ever read as a binding (N13/N14).
        let (_t, data, proj) = dirs();
        std::fs::create_dir_all(proj.join(".herdmesh")).unwrap();
        std::fs::write(proj.join(".herdmesh/room"), "evil-room\n").unwrap();
        assert_eq!(find_room(&data, &proj), None);
    }

    #[test]
    fn paths_differing_only_in_invalid_bytes_do_not_alias() {
        // Both lossy-convert to the same string with U+FFFD (GATE-5 N18).
        #[cfg(unix)]
        let (a, b) = {
            use std::os::unix::ffi::OsStrExt as _;
            (
                std::ffi::OsStr::from_bytes(b"/p/app\xff").to_owned(),
                std::ffi::OsStr::from_bytes(b"/p/app\xfe").to_owned(),
            )
        };
        #[cfg(windows)]
        let (a, b) = {
            use std::os::windows::ffi::OsStringExt as _;
            let mk = |lone: u16| std::ffi::OsString::from_wide(&[0x5c, 0x70, 0x5c, 0x61, lone]);
            (mk(0xD800), mk(0xD801))
        };
        assert_eq!(
            a.to_string_lossy(),
            b.to_string_lossy(),
            "premise: lossy strings collide"
        );
        assert_ne!(path_id(&a), path_id(&b));
        assert_ne!(path_id(&a)[..32], path_id(&b)[..32]);
    }

    #[test]
    fn a_binding_whose_recorded_identity_differs_is_rejected() {
        let (_t, data, proj) = dirs();
        bind(&data, &proj, R1).unwrap();
        let f = binding_file(&data, &project_key(&proj));
        let text = std::fs::read_to_string(&f).unwrap();
        let forged = text
            .lines()
            .map(|l| {
                if l.starts_with("path_id") {
                    format!("path_id = \"{}\"", "0".repeat(64))
                } else {
                    l.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&f, forged).unwrap();
        assert_eq!(bound_room(&data, &proj), None);
    }
}
