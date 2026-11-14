//! Reads and writes inside a project folder that cannot be redirected by links
//! (GATE-5 N13). Checking a path and then using the same path is a race: an
//! attacker who can write in the checkout can swap a folder for a link in
//! between. So the check and the use are one operation on a held handle:
//!
//! * Unix: every folder is opened with `O_NOFOLLOW | O_DIRECTORY` relative to
//!   the previous folder's fd (`openat`), files with `O_NOFOLLOW`, and the
//!   temp file is created and renamed relative to the verified folder fd
//!   (`renameat`). Nothing is ever resolved by a full pathname.
//! * Windows: every folder and file is opened with
//!   `FILE_FLAG_OPEN_REPARSE_POINT` (links are opened, not followed), the
//!   reparse bit is checked on the opened handle, and the handles of the whole
//!   chain stay open without `FILE_SHARE_DELETE`, so no folder on the way can
//!   be renamed, deleted or replaced while we use it.
//!
//! Writes go through the handle of a new exclusive temp file and a rename in
//! the same verified folder, so they can never land outside the root.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path};

/// Largest file we will read (config files are small).
const READ_MAX: u64 = 4 << 20;

fn refuse(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg.into())
}

/// `rel` as plain file-name components; anything else (`..`, absolute,
/// prefixes, empty) is refused.
fn parts(rel: &Path) -> io::Result<Vec<&OsStr>> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(n),
            _ => return Err(refuse(format!("unsafe path {}", rel.display()))),
        }
    }
    if out.is_empty() {
        return Err(refuse("empty path"));
    }
    Ok(out)
}

fn random_hex() -> io::Result<String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(unix)]
mod imp {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, mkdirat, openat, renameat, unlinkat};
    use rustix::io::Errno;
    use std::os::fd::OwnedFd;

    /// A folder held by file descriptor.
    pub struct Dir(OwnedFd);

    fn io_err(e: Errno) -> io::Error {
        io::Error::from(e)
    }

    pub fn open_root(path: &Path) -> io::Result<Dir> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io_err)?;
        Ok(Dir(fd))
    }

    impl Dir {
        /// `Ok(None)` when missing; an error for a link or a non-folder.
        pub fn child(&self, name: &OsStr) -> io::Result<Option<Dir>> {
            match openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => Ok(Some(Dir(fd))),
                Err(Errno::NOENT) => Ok(None),
                Err(Errno::LOOP | Errno::NOTDIR) => Err(refuse(format!(
                    "{} is a link or not a folder; refusing to follow links out of the project",
                    name.to_string_lossy()
                ))),
                Err(e) => Err(io_err(e)),
            }
        }

        pub fn mkdir(&self, name: &OsStr) -> io::Result<()> {
            mkdirat(
                &self.0,
                name,
                Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
            )
            .map_err(io_err)
        }

        /// Open an existing regular file for reading without following a link.
        pub fn open_read(&self, name: &OsStr) -> io::Result<Option<File>> {
            // NONBLOCK: a planted FIFO must not hang us; we reject it below.
            match openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => {
                    let st = fstat(&fd).map_err(io_err)?;
                    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                        return Err(refuse(format!(
                            "{} is not a regular file",
                            name.to_string_lossy()
                        )));
                    }
                    Ok(Some(File::from(fd)))
                }
                Err(Errno::NOENT) => Ok(None),
                Err(Errno::LOOP) => Err(refuse(format!(
                    "{} is a link; refusing to follow links out of the project",
                    name.to_string_lossy()
                ))),
                Err(e) => Err(io_err(e)),
            }
        }

        /// Create a new file (fails if the name exists, even as a link).
        pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
            let fd = openat(
                &self.0,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
            )
            .map_err(io_err)?;
            Ok(File::from(fd))
        }

        pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            renameat(&self.0, from, &self.0, to).map_err(io_err)
        }

        pub fn remove(&self, name: &OsStr) {
            let _ = unlinkat(&self.0, name, AtFlags::empty());
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::PathBuf;

    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const FILE_SHARE_DELETE: u32 = 4;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    /// A folder held by an open handle (no `FILE_SHARE_DELETE`, so it cannot
    /// be renamed or replaced while held). `path` is where it was opened.
    pub struct Dir {
        _handle: File,
        path: PathBuf,
    }

    fn open_dir(path: &Path) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    fn check_plain_dir(f: &File, name: &str) -> io::Result<()> {
        let md = f.metadata()?;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !md.is_dir() {
            return Err(refuse(format!(
                "{name} is a link or not a folder; refusing to follow links out of the project"
            )));
        }
        Ok(())
    }

    pub fn open_root(path: &Path) -> io::Result<Dir> {
        let h = open_dir(path)?;
        check_plain_dir(&h, &path.display().to_string())?;
        Ok(Dir {
            _handle: h,
            path: path.to_path_buf(),
        })
    }

    impl Dir {
        pub fn child(&self, name: &OsStr) -> io::Result<Option<Dir>> {
            let path = self.path.join(name);
            match open_dir(&path) {
                Ok(h) => {
                    check_plain_dir(&h, &name.to_string_lossy())?;
                    Ok(Some(Dir { _handle: h, path }))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        pub fn mkdir(&self, name: &OsStr) -> io::Result<()> {
            std::fs::create_dir(self.path.join(name))
        }

        pub fn open_read(&self, name: &OsStr) -> io::Result<Option<File>> {
            match std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path.join(name))
            {
                Ok(f) => {
                    let md = f.metadata()?;
                    if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(refuse(format!(
                            "{} is a link; refusing to follow links out of the project",
                            name.to_string_lossy()
                        )));
                    }
                    if !md.is_file() {
                        return Err(refuse(format!(
                            "{} is not a regular file",
                            name.to_string_lossy()
                        )));
                    }
                    Ok(Some(f))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }

        pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
            // FILE_SHARE_DELETE so the rename below works while we hold it.
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.path.join(name))
        }

        pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
            std::fs::rename(self.path.join(from), self.path.join(to))
        }

        pub fn remove(&self, name: &OsStr) {
            let _ = std::fs::remove_file(self.path.join(name));
        }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("herdmesh setup needs unix or windows file APIs");

use imp::Dir;

#[cfg(test)]
type Hook = Box<dyn Fn(&str)>;

/// A project folder anchored by a held handle.
pub struct Root {
    dir: Dir,
    #[cfg(test)]
    hook: Option<Hook>,
}

impl Root {
    /// Open `project` (its canonical form) as the anchor.
    pub fn open(project: &Path) -> io::Result<Self> {
        let canonical = std::fs::canonicalize(project)?;
        Ok(Self {
            dir: imp::open_root(&canonical)?,
            #[cfg(test)]
            hook: None,
        })
    }

    /// Test hook: called between a check and the use it protects, so tests
    /// can swap things on disk exactly where a race would.
    #[cfg(test)]
    fn with_hook(mut self, f: impl Fn(&str) + 'static) -> Self {
        self.hook = Some(Box::new(f));
        self
    }

    #[cfg(test)]
    fn at(&self, point: &str) {
        if let Some(h) = &self.hook {
            h(point);
        }
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn at(&self, _point: &str) {}

    /// The folders above `names`' last element, held for the whole operation.
    /// With `create`, missing folders are made one at a time.
    fn chain(&self, names: &[&OsStr], create: bool) -> io::Result<Option<Vec<Dir>>> {
        let mut held: Vec<Dir> = Vec::new();
        for (i, n) in names[..names.len() - 1].iter().enumerate() {
            self.at(&format!("open-dir:{i}"));
            let parent = held.last().unwrap_or(&self.dir);
            let child = match parent.child(n)? {
                Some(c) => c,
                None if create => {
                    parent.mkdir(n)?;
                    parent.child(n)?.ok_or_else(|| {
                        refuse(format!(
                            "{} vanished while being created",
                            n.to_string_lossy()
                        ))
                    })?
                }
                None => return Ok(None),
            };
            held.push(child);
        }
        Ok(Some(held))
    }

    /// The contents of `rel`, `None` if it does not exist. A link, or anything
    /// but a regular file, anywhere on the way is an error.
    pub fn read_to_string(&self, rel: &Path) -> io::Result<Option<String>> {
        let names = parts(rel)?;
        let Some(held) = self.chain(&names, false)? else {
            return Ok(None);
        };
        let dir = held.last().unwrap_or(&self.dir);
        self.at("open-file");
        let Some(file) = dir.open_read(names[names.len() - 1])? else {
            return Ok(None);
        };
        let mut text = String::new();
        file.take(READ_MAX + 1).read_to_string(&mut text)?;
        if text.len() as u64 > READ_MAX {
            return Err(refuse(format!("{} is too large", rel.display())));
        }
        Ok(Some(text))
    }

    /// Replace `rel` with `text` atomically: a new exclusive temp file in the
    /// verified folder, then a rename there. Missing folders are created.
    pub fn write_atomic(&self, rel: &Path, text: &str) -> io::Result<()> {
        let names = parts(rel)?;
        let held = self
            .chain(&names, true)?
            .ok_or_else(|| refuse("could not reach the target folder"))?;
        let dir = held.last().unwrap_or(&self.dir);
        let name = names[names.len() - 1];
        // An existing target must be a plain file (never a link to replace blindly).
        self.at("check-target");
        drop(dir.open_read(name)?);
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".{}.herdmesh-tmp", random_hex()?));
        let mut f = dir.create_new(&tmp_name)?;
        let result = (|| {
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            self.at("before-rename");
            dir.rename(&tmp_name, name)
        })();
        drop(f);
        if result.is_err() {
            dir.remove(&tmp_name);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }

    fn link_file(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
    }

    fn empty_tree(dir: &Path) -> bool {
        walk(dir).is_empty()
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            out.push(e.path());
            if e.file_type().unwrap().is_dir() {
                out.extend(walk(&e.path()));
            }
        }
        out
    }

    #[test]
    fn plain_reads_and_writes_work_and_leave_no_temp_files() {
        let t = tmp();
        let root = Root::open(t.path()).unwrap();
        assert_eq!(root.read_to_string(Path::new("a/b.txt")).unwrap(), None);
        root.write_atomic(Path::new("a/b.txt"), "one").unwrap();
        root.write_atomic(Path::new("a/b.txt"), "two").unwrap();
        assert_eq!(
            root.read_to_string(Path::new("a/b.txt"))
                .unwrap()
                .as_deref(),
            Some("two")
        );
        assert!(
            walk(t.path())
                .iter()
                .all(|p| !p.to_string_lossy().contains("herdmesh-tmp"))
        );
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let t = tmp();
        let root = Root::open(t.path()).unwrap();
        for bad in ["../x", "/abs", "a/../../x", ""] {
            assert!(root.read_to_string(Path::new(bad)).is_err(), "{bad}");
            assert!(root.write_atomic(Path::new(bad), "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn static_links_are_refused_for_reads_and_writes() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "SECRET").unwrap();
        if !link_dir(&p.join("d"), &outside) {
            eprintln!("SKIPPED static_links_are_refused: cannot create links");
            return;
        }
        let root = Root::open(&p).unwrap();
        assert!(root.read_to_string(Path::new("d/secret")).is_err());
        assert!(root.write_atomic(Path::new("d/new"), "x").is_err());
        assert!(!outside.join("new").exists());
    }

    /// The race: the check passes on the real folder, then a link is swapped in
    /// before the use. Whatever the platform does about the swap (refuse to
    /// move a held folder on Windows, keep using the original folder on Unix),
    /// nothing may be written outside and no secret may be read.
    #[test]
    fn folder_swapped_for_a_link_between_check_and_write_never_escapes() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(p.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let swapped = Rc::new(Cell::new(false));
        let (p2, o2, s2) = (p.clone(), outside.clone(), swapped.clone());
        let root = Root::open(&p).unwrap().with_hook(move |point| {
            if point != "check-target" || s2.get() {
                return;
            }
            s2.set(true);
            // Move the verified folder away and put a link in its place.
            if std::fs::rename(p2.join("sub"), p2.join("sub-moved")).is_ok() {
                let _ = link_dir(&p2.join("sub"), &o2);
            }
        });
        let result = root.write_atomic(Path::new("sub/CLAUDE.md"), "payload");
        assert!(swapped.get(), "hook must run");
        assert!(empty_tree(&outside), "wrote outside: {:?}", walk(&outside));
        // Either the folder could not be swapped (write landed in it), or it was
        // swapped and the write went to the original, moved folder.
        if result.is_ok() {
            let in_place = p.join("sub/CLAUDE.md");
            let moved = p.join("sub-moved/CLAUDE.md");
            assert!(in_place.is_file() || moved.is_file());
        }
    }

    #[test]
    fn file_swapped_for_a_link_before_it_is_opened_is_not_read() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret");
        std::fs::write(&secret, "TOP-SECRET").unwrap();
        std::fs::write(p.join("CLAUDE.md"), "innocent").unwrap();
        if !link_file(&p.join("probe"), &secret) {
            eprintln!("SKIPPED file_swapped_for_a_link: cannot create file links");
            return;
        }
        std::fs::remove_file(p.join("probe")).unwrap();
        let (p2, s2) = (p.clone(), secret.clone());
        let root = Root::open(&p).unwrap().with_hook(move |point| {
            if point == "open-file" {
                let _ = std::fs::remove_file(p2.join("CLAUDE.md"));
                let _ = link_file(&p2.join("CLAUDE.md"), &s2);
            }
        });
        let got = root.read_to_string(Path::new("CLAUDE.md"));
        assert!(got.is_err(), "a link must be refused, got {got:?}");
    }

    #[test]
    fn a_planted_temp_link_cannot_be_written_through() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        if !link_file(&p.join(".f.herdmesh-tmp"), &victim) {
            eprintln!("SKIPPED a_planted_temp_link: cannot create file links");
            return;
        }
        let root = Root::open(&p).unwrap();
        root.write_atomic(Path::new("f"), "new").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(p.join("f")).unwrap(), "new");
    }
}
