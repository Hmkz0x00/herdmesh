//! The room a project folder belongs to. `herdmesh setup` records its local
//! room name in `.herdmesh/room`; room commands fall back to it when the room
//! argument is omitted (searching the current folder and its parents).

use std::io;
use std::path::{Path, PathBuf};

pub const DIR: &str = ".herdmesh";
pub const FILE: &str = "room";
const NAME_MAX: usize = 64;

fn path_in(dir: &Path) -> PathBuf {
    dir.join(DIR).join(FILE)
}

/// Record `name` as the project's room.
pub fn write_room(project: &Path, name: &str) -> io::Result<()> {
    let p = path_in(project);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, format!("{name}\n"))
}

/// The recorded room of `start` or the nearest parent that has one.
pub fn find_room(start: &Path) -> Option<String> {
    start.ancestors().find_map(|dir| {
        let text = std::fs::read_to_string(path_in(dir)).ok()?;
        let name = text.lines().next()?.trim();
        (!name.is_empty() && name.len() <= NAME_MAX && !name.chars().any(char::is_control))
            .then(|| name.to_owned())
    })
}

/// An explicit room wins; otherwise the project's room; otherwise an error
/// that says how to fix it.
pub fn room_arg(given: Option<&str>, cwd: &Path) -> Result<String, String> {
    if let Some(g) = given {
        return Ok(g.to_owned());
    }
    find_room(cwd).ok_or_else(|| {
        "no room given and this folder has no herdmesh room; run `herdmesh setup` here or pass the room name"
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_room_in_the_folder_and_in_parents() {
        let t = tempfile::tempdir().unwrap();
        let sub = t.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_room(&sub), None);
        write_room(t.path(), "myproj").unwrap();
        assert_eq!(find_room(t.path()).as_deref(), Some("myproj"));
        assert_eq!(find_room(&sub).as_deref(), Some("myproj"));
    }

    #[test]
    fn nearest_binding_wins_and_junk_is_ignored() {
        let t = tempfile::tempdir().unwrap();
        let inner = t.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        write_room(t.path(), "outer").unwrap();
        write_room(&inner, "innerroom").unwrap();
        assert_eq!(find_room(&inner).as_deref(), Some("innerroom"));
        std::fs::write(path_in(&inner), "\n").unwrap();
        assert_eq!(find_room(&inner).as_deref(), Some("outer"));
    }

    #[test]
    fn explicit_room_beats_project_room_and_missing_is_a_helpful_error() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(room_arg(Some("x"), t.path()).unwrap(), "x");
        let e = room_arg(None, t.path()).unwrap_err();
        assert!(e.contains("herdmesh setup"), "{e}");
        write_room(t.path(), "p").unwrap();
        assert_eq!(room_arg(None, t.path()).unwrap(), "p");
        assert_eq!(room_arg(Some("q"), t.path()).unwrap(), "q");
    }
}
