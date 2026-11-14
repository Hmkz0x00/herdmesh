//! `herdmesh setup`: one command per person per project (ADR-004 section 2).
//!
//! Project-level files only, never user-level config:
//! * `.mcp.json` (Claude Code), `.codex/config.toml` (Codex),
//!   `.agents/mcp_config.json` (Antigravity), each running
//!   `herdmesh mcp --kind <kind> --room <room>`, merged into what is there;
//! * the collaboration snippet appended to `CLAUDE.md` / `AGENTS.md`;
//! * `.herdmesh/room`, so room commands know the project's room.
//!
//! Every file change is shown as a diff and needs a yes unless `--yes`.
//! Re-running changes nothing once everything is in place.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Component, Path, PathBuf};

use hm_net::RoomTicket;
use serde_json::{Value, json};

use crate::clipboard;
use crate::daemon;
use crate::data::DataDir;
use crate::naming;
use crate::project;
use crate::room;

const SNIPPET_DOC: &str = include_str!("../../../docs/snippets/herdmesh-collab.md");
/// Heading of the snippet; its presence means the snippet is already installed.
pub const SNIPPET_MARKER: &str = "## Herdmesh Collaboration";
/// Name of the server entry written into every agent config.
pub const SERVER_NAME: &str = "herdmesh";

/// An agent CLI we can configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Claude,
    Codex,
    Agy,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Claude, Kind::Codex, Kind::Agy];

    /// Used both as the executable name and as `--kind`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Agy => "agy",
        }
    }

    /// Project-relative config file, as path components.
    fn config_parts(self) -> &'static [&'static str] {
        match self {
            Kind::Claude => &[".mcp.json"],
            Kind::Codex => &[".codex", "config.toml"],
            Kind::Agy => &[".agents", "mcp_config.json"],
        }
    }

    pub fn config_path(self) -> PathBuf {
        self.config_parts().iter().collect()
    }

    fn is_toml(self) -> bool {
        self == Kind::Codex
    }
}

/// The collaboration snippet: the fenced markdown block of the snippet doc.
pub fn snippet() -> String {
    let doc = SNIPPET_DOC.replace("\r\n", "\n");
    let fence = "```markdown\n";
    let Some(start) = doc.find(fence).map(|i| i + fence.len()) else {
        return String::new();
    };
    let end = doc[start..].find("\n```").map_or(doc.len(), |i| start + i);
    doc[start..end].trim_end().to_owned() + "\n"
}

/// Which agent CLIs are installed: an executable named after each kind in a
/// `PATH` directory (trying each of `exts`, e.g. `.exe` on Windows).
pub fn detect(path_var: &OsStr, exts: &[&str]) -> Vec<Kind> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path_var).collect();
    Kind::ALL
        .into_iter()
        .filter(|k| {
            dirs.iter().any(|d| {
                exts.iter()
                    .any(|e| d.join(format!("{}{e}", k.name())).is_file())
            })
        })
        .collect()
}

/// Installed agent CLIs on this machine's `PATH`.
pub fn detect_installed() -> Vec<Kind> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ".com"]
    } else {
        &[""]
    };
    std::env::var_os("PATH").map_or_else(Vec::new, |p| detect(&p, exts))
}

fn server_args(kind: Kind, room: &str) -> Vec<String> {
    ["mcp", "--kind", kind.name(), "--room", room]
        .map(str::to_owned)
        .to_vec()
}

fn server_entry_json(kind: Kind, room: &str) -> Value {
    json!({ "command": "herdmesh", "args": server_args(kind, room) })
}

/// `existing` JSON config with our server entry set; everything else kept.
pub fn merge_json(existing: Option<&str>, kind: Kind, room: &str) -> Result<String, String> {
    let mut root: Value = match existing {
        Some(t) if !t.trim().is_empty() => {
            serde_json::from_str(t).map_err(|e| format!("not valid JSON ({e})"))?
        }
        _ => json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or("top level is not a JSON object")?;
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or("\"mcpServers\" is not an object")?;
    servers.insert(SERVER_NAME.to_owned(), server_entry_json(kind, room));
    let mut text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

fn server_table_toml(kind: Kind, room: &str) -> toml::Table {
    let mut entry = toml::Table::new();
    entry.insert("command".into(), "herdmesh".into());
    entry.insert(
        "args".into(),
        toml::Value::Array(
            server_args(kind, room)
                .into_iter()
                .map(toml::Value::String)
                .collect(),
        ),
    );
    let mut servers = toml::Table::new();
    servers.insert(SERVER_NAME.into(), toml::Value::Table(entry));
    let mut root = toml::Table::new();
    root.insert("mcp_servers".into(), toml::Value::Table(servers));
    root
}

/// `existing` Codex TOML config with our server entry set. A missing entry is
/// appended as text so comments and layout survive; a differing entry is
/// replaced by re-serialising the file.
pub fn merge_toml(existing: Option<&str>, kind: Kind, room: &str) -> Result<String, String> {
    let wanted = server_table_toml(kind, room);
    let text = existing.unwrap_or("");
    let mut parsed: toml::Table = if text.trim().is_empty() {
        toml::Table::new()
    } else {
        text.parse().map_err(|e| format!("not valid TOML ({e})"))?
    };
    let has_entry = parsed
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .is_some_and(|t| t.contains_key(SERVER_NAME));
    if !has_entry {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&toml::to_string(&wanted).map_err(|e| e.to_string())?);
        if out.parse::<toml::Table>().is_ok() {
            return Ok(out);
        }
    }
    // Replace (or the append produced invalid TOML, e.g. inline tables).
    let servers = parsed
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or("\"mcp_servers\" is not a table")?;
    let entry = wanted
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(SERVER_NAME))
        .cloned()
        .ok_or("internal: missing entry")?;
    servers.insert(SERVER_NAME.to_owned(), entry);
    toml::to_string(&parsed).map_err(|e| e.to_string())
}

/// True when both texts hold the same data (formatting differences ignored).
fn same_data(kind: Kind, old: &str, new: &str) -> bool {
    if kind.is_toml() {
        matches!((old.parse::<toml::Table>(), new.parse::<toml::Table>()), (Ok(a), Ok(b)) if a == b)
    } else {
        matches!((serde_json::from_str::<Value>(old), serde_json::from_str::<Value>(new)), (Ok(a), Ok(b)) if a == b)
    }
}

/// Minimal unified-style diff for showing the user what will change.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = format!("--- {path}\n+++ {path} (after setup)\n");
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        let _ = writeln!(
            out,
            "(file too large to diff; {} lines will be replaced)",
            a.len()
        );
        return out;
    }
    // LCS table over lines.
    let mut t = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            t[i][j] = if a[i] == b[j] {
                t[i + 1][j + 1] + 1
            } else {
                t[i + 1][j].max(t[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if t[i + 1][j] >= t[i][j + 1] {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| ('-', *l)));
    ops.extend(b[j..].iter().map(|l| ('+', *l)));
    const CONTEXT: usize = 2;
    let keep: Vec<bool> = (0..ops.len())
        .map(|k| {
            let lo = k.saturating_sub(CONTEXT);
            let hi = (k + CONTEXT).min(ops.len().saturating_sub(1));
            ops[lo..=hi].iter().any(|(c, _)| *c != ' ')
        })
        .collect();
    let mut gap = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if keep[k] {
            let _ = writeln!(out, "{c} {line}");
            gap = false;
        } else if !gap {
            out.push_str("  ...\n");
            gap = true;
        }
    }
    out
}

/// One file we would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Project-relative path.
    pub path: PathBuf,
    pub old: Option<String>,
    pub new: String,
}

impl FileChange {
    pub fn diff(&self) -> String {
        unified_diff(
            &self.path.display().to_string(),
            self.old.as_deref().unwrap_or(""),
            &self.new,
        )
    }
}

/// What setup would change, plus files it refuses to touch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<FileChange>,
    /// `(path, reason)` for files we could not merge safely (left alone).
    pub problems: Vec<(PathBuf, String)>,
}

fn read_opt(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.replace("\r\n", "\n"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn refuse(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, msg.into())
}

/// Symlinks, and on Windows junctions and every other reparse point.
fn is_link(md: &std::fs::Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// `root/rel` for reading or writing, or an error. `root` must be canonical.
/// Every existing component below the root must be a real folder (the last
/// one a regular file): a link or reparse point anywhere on the way is
/// refused, so nothing is ever read or written outside the project through
/// one (GATE-4 N13). A missing tail is fine.
fn checked_target(root: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    let comps: Vec<_> = rel.components().collect();
    if comps.is_empty() || comps.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(refuse(format!("unsafe path {}", rel.display())));
    }
    let mut cur = root.to_path_buf();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) => {
                if is_link(&md) {
                    return Err(refuse(format!(
                        "{} is a link; refusing to follow links out of the project",
                        rel.display()
                    )));
                }
                let last = i + 1 == comps.len();
                if last && !md.is_file() {
                    return Err(refuse(format!("{} is not a regular file", rel.display())));
                }
                if !last && !md.is_dir() {
                    return Err(refuse(format!("{} is not a folder", cur.display())));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    // Belt and braces: the deepest existing part must resolve inside the root.
    let full = root.join(rel);
    if let Some(existing) = full
        .ancestors()
        .find(|a| std::fs::symlink_metadata(a).is_ok())
    {
        let real = std::fs::canonicalize(existing)?;
        if !real.starts_with(root) {
            return Err(refuse(format!(
                "{} resolves outside the project",
                rel.display()
            )));
        }
    }
    Ok(full)
}

/// Create the folders above `rel` one by one, never through a link.
fn create_parents(root: &Path, rel: &Path) -> std::io::Result<()> {
    let Some(parent) = rel.parent() else {
        return Ok(());
    };
    let mut cur = root.to_path_buf();
    for c in parent.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if is_link(&md) || !md.is_dir() => {
                return Err(refuse(format!("{} is not a plain folder", cur.display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur)?;
                let md = std::fs::symlink_metadata(&cur)?;
                if is_link(&md) || !md.is_dir() {
                    return Err(refuse(format!("{} is not a plain folder", cur.display())));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn random_hex() -> std::io::Result<String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Write `text` to `root/rel` atomically: an exclusive, unpredictably named
/// temp file in the same folder, then a rename. A pre-planted link with the
/// temp name makes `create_new` fail instead of truncating its target, and
/// the rename replaces a (refused) link rather than writing through it.
fn write_checked(root: &Path, rel: &Path, text: &str) -> std::io::Result<()> {
    checked_target(root, rel)?;
    create_parents(root, rel)?;
    let target = checked_target(root, rel)?;
    let dir = target
        .parent()
        .ok_or_else(|| refuse("path has no parent"))?;
    let name = target
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| refuse("path has no file name"))?;
    let tmp = dir.join(format!(".{name}.{}.herdmesh-tmp", random_hex()?));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        // Last check right before the swap (narrows, cannot close, a race).
        checked_target(root, rel)?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Like `read_opt`, but through `checked_target`: a link is an error, so a
/// planted `CLAUDE.md -> ~/.ssh/...` is never read into a diff.
fn read_checked(root: &Path, rel: &Path) -> Result<Option<String>, String> {
    let path = checked_target(root, rel).map_err(|e| e.to_string())?;
    read_opt(&path)
}

/// Which instruction file gets the snippet for `kinds`.
fn instruction_targets(project: &Path, kinds: &[Kind]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for k in kinds {
        let t = match k {
            Kind::Claude => {
                // CLAUDE.md that imports AGENTS.md is satisfied by AGENTS.md.
                let imports = read_checked(project, Path::new("CLAUDE.md"))
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.lines().any(|l| l.trim() == "@AGENTS.md"));
                if imports { "AGENTS.md" } else { "CLAUDE.md" }
            }
            Kind::Codex | Kind::Agy => "AGENTS.md",
        };
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Work out the file changes for `kinds` in `project` without writing anything.
pub fn plan_files(project: &Path, kinds: &[Kind], room: &str) -> Plan {
    let mut plan = Plan::default();
    let project = match std::fs::canonicalize(project) {
        Ok(p) => p,
        Err(e) => {
            plan.problems
                .push((PathBuf::from("."), format!("project folder: {e}")));
            return plan;
        }
    };
    let project = project.as_path();
    for &kind in kinds {
        let rel = kind.config_path();
        let result = read_checked(project, &rel).and_then(|old| {
            let new = if kind.is_toml() {
                merge_toml(old.as_deref(), kind, room)
            } else {
                merge_json(old.as_deref(), kind, room)
            }?;
            Ok((old, new))
        });
        match result {
            Ok((Some(old), new)) if same_data(kind, &old, &new) => {}
            Ok((old, new)) => plan.changes.push(FileChange {
                path: rel,
                old,
                new,
            }),
            Err(why) => plan.problems.push((rel, why)),
        }
    }
    let snip = snippet();
    for target in instruction_targets(project, kinds) {
        let rel = PathBuf::from(target);
        match read_checked(project, &rel) {
            Ok(Some(old)) if old.contains(SNIPPET_MARKER) => {}
            Ok(old) => {
                let mut new = old.clone().unwrap_or_default();
                if !new.is_empty() {
                    if !new.ends_with('\n') {
                        new.push('\n');
                    }
                    new.push('\n');
                }
                new.push_str(&snip);
                plan.changes.push(FileChange {
                    path: rel,
                    old,
                    new,
                });
            }
            Err(why) => plan.problems.push((rel, why)),
        }
    }
    plan
}

/// Result of applying a plan.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub written: Vec<PathBuf>,
    pub declined: Vec<PathBuf>,
    /// `(path, reason)` for files that were approved but could not be written
    /// safely (for example a link appeared); nothing was written for them.
    pub failed: Vec<(PathBuf, String)>,
}

/// Show each change's diff via `show`, ask `confirm`, write the accepted
/// ones. Writes never go through links and are re-checked at write time.
pub fn apply(
    project: &Path,
    plan: &Plan,
    confirm: &mut dyn FnMut(&FileChange) -> bool,
    show: &mut dyn FnMut(&str),
) -> std::io::Result<Applied> {
    let mut done = Applied::default();
    if plan.changes.is_empty() {
        return Ok(done);
    }
    let root = std::fs::canonicalize(project)?;
    for ch in &plan.changes {
        show(&ch.diff());
        if !confirm(ch) {
            done.declined.push(ch.path.clone());
            continue;
        }
        match write_checked(&root, &ch.path, &ch.new) {
            Ok(()) => done.written.push(ch.path.clone()),
            Err(e) => done.failed.push((ch.path.clone(), e.to_string())),
        }
    }
    Ok(done)
}

/// `y`/`yes` (any case) at the prompt.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn ask_yes(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && is_yes(&line)
}

/// Local room name for a project folder: the folder name as a valid name.
pub fn room_name_for(project: &Path) -> String {
    let folder = project
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    hm_proto::names::sanitize_name(folder, hm_proto::names::NAME_MAX)
        .unwrap_or_else(|| "project".to_owned())
}

/// A local room name for this folder that no existing room uses. The plain
/// folder name when free; otherwise the name plus the folder's identity, so
/// two folders called `app` never share a room (N14).
fn unique_room_name(
    base: &str,
    rooms: &[(hm_proto::RoomId, crate::data::RoomConfig)],
    key: &str,
) -> Result<String, String> {
    let taken = |n: &str| rooms.iter().any(|(_, c)| c.name == n);
    if !taken(base) {
        return Ok(base.to_owned());
    }
    let mut short = base.to_owned();
    short.truncate(hm_proto::names::NAME_MAX - 9);
    let alt = format!("{}-{}", short.trim_end_matches('-'), &key[..8]);
    if taken(&alt) {
        return Err(format!(
            "a room named '{base}' exists and so does '{alt}'; pick another folder or remove one of the rooms"
        ));
    }
    println!(
        "A room named '{base}' already exists but is not bound to this folder; creating '{alt}' for it."
    );
    Ok(alt)
}

pub struct Opts<'a> {
    pub data: &'a DataDir,
    pub project: &'a Path,
    /// `Some(ticket)`, or `Some("")` to read the ticket from the clipboard.
    pub join: Option<String>,
    pub name: Option<String>,
    pub yes: bool,
}

/// Run setup, printing progress to stdout. Returns an error message on failure.
pub async fn run(opts: Opts<'_>) -> Result<(), String> {
    let interactive = std::io::stdin().is_terminal();
    if !opts.yes && !interactive {
        return Err(
            "setup asks before changing files, and this is not a terminal; add --yes".to_owned(),
        );
    }
    let data = opts.data;
    // 1. person name.
    let mut cfg = data.load_config().map_err(|e| e.to_string())?;
    if let Some(n) = &opts.name {
        cfg.person = Some(naming::validate("--name", n).map_err(|e| e.to_string())?);
        data.save_config(&cfg).map_err(|e| e.to_string())?;
    }
    let person = naming::person(&cfg).map_err(|e| e.to_string())?;
    println!("You are '{person}' (change with `herdmesh setup --name <you>`).");

    // 2. the room, chosen by the folder's identity, never by its name alone.
    let root = project::canonical_root(opts.project)
        .map_err(|e| format!("project folder {}: {e}", opts.project.display()))?;
    let key = project::project_key(&root);
    let cli_agent = naming::candidate(&person, "cli", 1).map_err(|e| e.to_string())?;
    let rooms = data.list_rooms().map_err(|e| e.to_string())?;
    let ticket: Option<RoomTicket> = match &opts.join {
        None => None,
        Some(arg) => {
            let raw = if arg.trim().is_empty() {
                clipboard::paste()
                    .ok_or("no ticket given and the clipboard has none; pass --join <ticket>")?
            } else {
                arg.clone()
            };
            Some(
                raw.trim()
                    .parse::<RoomTicket>()
                    .map_err(|e| format!("invalid ticket: {e}"))?,
            )
        }
    };
    let bound = project::bound_room(data.root(), &root)
        .and_then(|id| rooms.iter().find(|(r, _)| r.to_string() == id).cloned());
    let mut ticket_to_share: Option<RoomTicket> = None;
    let (room_id, room_name, joined) = match (&bound, &ticket) {
        (Some((id, cfg)), t) => {
            if let Some(t) = t.as_ref().filter(|t| t.room_id != *id) {
                return Err(format!(
                    "this folder is already bound to room '{}' ({id}), but the ticket is for room {}; \
                     run setup without --join, or use a different folder",
                    cfg.name, t.room_id
                ));
            }
            println!(
                "This folder is already bound to room '{}'; reusing it.",
                cfg.name
            );
            (*id, cfg.name.clone(), false)
        }
        (None, Some(t)) => {
            if let Some((id, cfg)) = rooms.iter().find(|(r, _)| *r == t.room_id) {
                println!(
                    "You already have room '{}'; binding this folder to it.",
                    cfg.name
                );
                (*id, cfg.name.clone(), true)
            } else {
                let name = unique_room_name(&room_name_for(&root), &rooms, &key)?;
                let out = room::join(data, &t.to_string(), &cli_agent, Some(&name))
                    .map_err(|e| e.to_string())?;
                print!("{out}");
                (t.room_id, name, true)
            }
        }
        (None, None) => {
            let name = unique_room_name(&room_name_for(&root), &rooms, &key)?;
            let made = room::create_with_ticket(data, &name, &cli_agent)
                .await
                .map_err(|e| e.to_string())?;
            println!("Created room '{name}' (id: {}).", made.id);
            let id = made.id;
            ticket_to_share = Some(made.ticket);
            (id, name, false)
        }
    };
    project::bind(data.root(), &root, &room_id.to_string())
        .map_err(|e| format!("could not record the folder-to-room binding: {e}"))?;
    println!(
        "Folder {} is bound to room '{room_name}' (id {room_id}).",
        root.display()
    );
    let room_name = room_name.as_str();

    // 3. daemon (warning only: the agents start it too).
    if let Err(e) = daemon::ensure_for_cli(
        data,
        &daemon::default_socket(),
        || daemon::spawn_detached(data),
        |m| eprintln!("herdmesh setup: {m}"),
    )
    .await
    {
        eprintln!("herdmesh setup: daemon not started ({e}); it starts with your first agent");
    }

    // 4. project files.
    let kinds = detect_installed();
    if kinds.is_empty() {
        println!("No agent CLI (claude, codex, agy) found on PATH; skipped MCP config.");
    } else {
        let names: Vec<&str> = kinds.iter().map(|k| k.name()).collect();
        println!("Found: {}.", names.join(", "));
    }
    let plan = plan_files(&root, &kinds, room_name);
    for (path, why) in &plan.problems {
        eprintln!("herdmesh setup: leaving {} alone: {why}", path.display());
    }
    let yes = opts.yes;
    let applied = apply(
        &root,
        &plan,
        &mut |ch| {
            yes || ask_yes(&format!(
                "{} {}?",
                if ch.old.is_some() { "Update" } else { "Create" },
                ch.path.display()
            ))
        },
        &mut |d| print!("{d}"),
    )
    .map_err(|e| e.to_string())?;
    for p in &applied.written {
        println!("wrote {}", p.display());
    }
    for p in &applied.declined {
        println!("skipped {}", p.display());
    }
    for (p, why) in &applied.failed {
        eprintln!("herdmesh setup: did not write {}: {why}", p.display());
    }
    if plan.changes.is_empty() && plan.problems.is_empty() {
        println!("Project files are already set up.");
    }

    // 5. ticket and next steps.
    if let Some(t) = &ticket_to_share {
        let text = t.to_string();
        println!("\nInvite ticket (a secret: share only with people who should join):\n{text}");
        match clipboard::copy(&text) {
            Some(_) => println!("(copied to the clipboard)"),
            None => println!("(no clipboard tool found; copy it by hand)"),
        }
    }
    println!("\nNext:");
    match (&ticket_to_share, joined) {
        (Some(_), _) => println!(
            "  1. Send the ticket to your teammate; they run `herdmesh setup --join` in their copy of this project."
        ),
        (None, true) => {
            println!("  1. You joined; keep `herdmesh room run` going (or just start your agent).")
        }
        (None, false) => {
            println!("  1. Need a ticket for someone else? `herdmesh room invite --copy`.")
        }
    }
    println!(
        "  2. Start your agent here (claude / codex / agy); it picks up the herdmesh MCP server."
    );
    println!("  3. Watch it live with `herdmesh view`.");
    if !applied.failed.is_empty() {
        return Err("some project files were not written (see above)".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn all(plan: &Plan, project: &Path) -> Applied {
        apply(project, plan, &mut |_| true, &mut |_| {}).unwrap()
    }

    #[test]
    fn snippet_comes_from_the_docs_block() {
        let s = snippet();
        assert!(s.starts_with(SNIPPET_MARKER), "{s}");
        assert!(s.contains("`claim`") && !s.contains("```"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn detects_only_installed_clis() {
        let t = tmp();
        std::fs::write(t.path().join("codex.exe"), "").unwrap();
        std::fs::write(t.path().join("claude"), "").unwrap();
        std::fs::create_dir(t.path().join("agy")).unwrap(); // a folder is not a CLI
        let path = std::env::join_paths([t.path()]).unwrap();
        assert_eq!(
            detect(&path, &["", ".exe"]),
            vec![Kind::Claude, Kind::Codex]
        );
        assert_eq!(detect(&path, &[".cmd"]), vec![]);
    }

    #[test]
    fn json_merge_keeps_other_servers_and_replaces_ours() {
        let old =
            r#"{"mcpServers":{"other":{"command":"x"},"herdmesh":{"command":"old"}},"keep":1}"#;
        let new = merge_json(Some(old), Kind::Claude, "proj").unwrap();
        let v: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(v["keep"], 1);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["herdmesh"]["command"], "herdmesh");
        assert_eq!(
            v["mcpServers"]["herdmesh"]["args"],
            json!(["mcp", "--kind", "claude", "--room", "proj"])
        );
        assert!(new.ends_with('\n'));
        assert!(merge_json(Some("{not json"), Kind::Claude, "p").is_err());
        assert!(merge_json(Some("[1]"), Kind::Claude, "p").is_err());
        assert!(merge_json(Some(r#"{"mcpServers":3}"#), Kind::Claude, "p").is_err());
        assert!(merge_json(None, Kind::Agy, "p").unwrap().contains("--kind"));
    }

    #[test]
    fn toml_merge_appends_and_keeps_comments() {
        let old = "# my settings\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"y\"\n";
        let new = merge_toml(Some(old), Kind::Codex, "proj").unwrap();
        assert!(new.starts_with(old), "{new}");
        let t: toml::Table = new.parse().unwrap();
        let ours = &t["mcp_servers"]["herdmesh"];
        assert_eq!(ours["command"].as_str(), Some("herdmesh"));
        assert_eq!(ours["args"].as_array().unwrap().len(), 5);
        assert!(t["mcp_servers"]["other"].is_table());
    }

    #[test]
    fn toml_merge_replaces_a_stale_entry_and_handles_odd_layouts() {
        let stale = "[mcp_servers.herdmesh]\ncommand = \"old\"\nargs = []\n";
        let t: toml::Table = merge_toml(Some(stale), Kind::Codex, "r")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            t["mcp_servers"]["herdmesh"]["command"].as_str(),
            Some("herdmesh")
        );
        // Inline table: appending a [mcp_servers.herdmesh] header would be invalid TOML.
        let inline = "mcp_servers = { other = { command = \"y\" } }\n";
        let t: toml::Table = merge_toml(Some(inline), Kind::Codex, "r")
            .unwrap()
            .parse()
            .unwrap();
        assert!(t["mcp_servers"]["other"].is_table() && t["mcp_servers"]["herdmesh"].is_table());
        assert!(merge_toml(Some("= broken"), Kind::Codex, "r").is_err());
        assert!(
            merge_toml(None, Kind::Codex, "r")
                .unwrap()
                .contains("[mcp_servers.herdmesh]")
        );
    }

    #[test]
    fn diff_marks_only_changes_with_context() {
        let d = unified_diff("f", "a\nb\nc\nd\ne\nf\ng\n", "a\nb\nc\nX\ne\nf\ng\n");
        assert!(d.contains("- d\n") && d.contains("+ X\n"), "{d}");
        assert!(d.contains("  c\n") && !d.contains("  a\n"), "{d}");
        assert!(d.contains("  ...\n"));
        let created = unified_diff("n", "", "one\ntwo\n");
        assert!(created.contains("+ one\n+ two\n"));
    }

    #[test]
    fn instruction_target_follows_agents_md_imports() {
        let t = tmp();
        let root = std::fs::canonicalize(t.path()).unwrap();
        assert_eq!(
            instruction_targets(&root, &[Kind::Claude]),
            vec!["CLAUDE.md"]
        );
        assert_eq!(
            instruction_targets(&root, &[Kind::Claude, Kind::Codex, Kind::Agy]),
            vec!["CLAUDE.md", "AGENTS.md"]
        );
        std::fs::write(t.path().join("CLAUDE.md"), "# x\n@AGENTS.md\n").unwrap();
        assert_eq!(
            instruction_targets(&root, &[Kind::Claude, Kind::Codex]),
            vec!["AGENTS.md"]
        );
    }

    #[test]
    fn writes_merges_and_is_idempotent() {
        let t = tmp();
        let p = t.path();
        std::fs::write(
            p.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(p.join("AGENTS.md"), "# Project rules\n- be nice").unwrap();
        let kinds = [Kind::Claude, Kind::Codex, Kind::Agy];

        let plan = plan_files(p, &kinds, "proj");
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        let done = all(&plan, p);
        assert_eq!(done.written.len(), 5, "{done:?}"); // 3 configs + CLAUDE.md + AGENTS.md

        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
        assert_eq!(mcp["mcpServers"]["herdmesh"]["args"][2], "claude");
        let codex = std::fs::read_to_string(p.join(".codex/config.toml")).unwrap();
        assert!(codex.contains("--kind") && codex.contains("codex"));
        assert!(
            std::fs::read_to_string(p.join(".agents/mcp_config.json"))
                .unwrap()
                .contains("agy")
        );
        let agents = std::fs::read_to_string(p.join("AGENTS.md")).unwrap();
        assert!(
            agents.starts_with("# Project rules\n- be nice\n\n## Herdmesh Collaboration"),
            "{agents}"
        );
        assert!(
            std::fs::read_to_string(p.join("CLAUDE.md"))
                .unwrap()
                .starts_with(SNIPPET_MARKER)
        );

        // Second run: nothing to do, and the files are byte-identical.
        let before: Vec<String> = [".mcp.json", ".codex/config.toml", "AGENTS.md", "CLAUDE.md"]
            .iter()
            .map(|f| std::fs::read_to_string(p.join(f)).unwrap())
            .collect();
        let again = plan_files(p, &kinds, "proj");
        assert_eq!(again, Plan::default());
        assert_eq!(all(&again, p), Applied::default());
        let after: Vec<String> = [".mcp.json", ".codex/config.toml", "AGENTS.md", "CLAUDE.md"]
            .iter()
            .map(|f| std::fs::read_to_string(p.join(f)).unwrap())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn a_changed_room_updates_only_the_entry() {
        let t = tmp();
        let p = t.path();
        all(&plan_files(p, &[Kind::Claude], "one"), p);
        let plan = plan_files(p, &[Kind::Claude], "two");
        let cfg: Vec<_> = plan
            .changes
            .iter()
            .filter(|c| c.path.ends_with(".mcp.json"))
            .collect();
        assert_eq!(cfg.len(), 1);
        assert!(cfg[0].diff().contains("two"));
    }

    #[test]
    fn declining_writes_nothing_and_broken_files_are_left_alone() {
        let t = tmp();
        let p = t.path();
        std::fs::write(p.join(".mcp.json"), "{oops").unwrap();
        let plan = plan_files(p, &[Kind::Claude], "r");
        assert_eq!(plan.problems.len(), 1);
        assert_eq!(plan.problems[0].0, Path::new(".mcp.json"));
        let mut shown = String::new();
        let done = apply(p, &plan, &mut |_| false, &mut |d| shown.push_str(d)).unwrap();
        assert!(done.written.is_empty() && done.declined == vec![PathBuf::from("CLAUDE.md")]);
        assert!(shown.contains("+++ CLAUDE.md"));
        assert!(!p.join("CLAUDE.md").exists());
        assert_eq!(
            std::fs::read_to_string(p.join(".mcp.json")).unwrap(),
            "{oops"
        );
    }

    #[test]
    fn never_touches_user_level_config() {
        let t = tmp();
        let plan = plan_files(t.path(), &Kind::ALL, "r");
        for c in &plan.changes {
            assert!(c.path.is_relative(), "{}", c.path.display());
            assert!(!c.path.components().any(|x| x.as_os_str() == ".."));
        }
    }

    #[test]
    fn room_name_follows_the_folder() {
        assert_eq!(room_name_for(Path::new("/x/My Project")), "my-project");
        assert_eq!(room_name_for(Path::new("/x/123")), "project");
        assert_eq!(room_name_for(Path::new("/")), "project");
    }

    #[test]
    fn yes_answers() {
        assert!(is_yes("Y\n") && is_yes(" yes ") && !is_yes("") && !is_yes("no") && !is_yes("yep"));
    }

    // ---- GATE-4 N13: links ----

    /// A folder link: symlink on unix, junction (no privilege needed) on Windows.
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

    macro_rules! skip_unless {
        ($ok:expr, $name:expr) => {
            if !$ok {
                eprintln!(
                    "SKIPPED {}: this OS or user may not create links in tests",
                    $name
                );
                return;
            }
        };
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            out.push(e.path());
            if e.file_type().unwrap().is_dir() {
                out.extend(files_under(&e.path()));
            }
        }
        out
    }

    #[test]
    fn a_linked_config_folder_is_never_written_through() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        skip_unless!(link_dir(&p.join(".codex"), &outside), "linked .codex");

        // Planning reports the link and plans nothing for it.
        let plan = plan_files(&p, &[Kind::Codex], "r");
        assert!(
            plan.problems
                .iter()
                .any(|(f, why)| f.ends_with("config.toml") && why.contains("link")),
            "{:?}",
            plan.problems
        );
        assert!(plan.changes.iter().all(|c| !c.path.starts_with(".codex")));

        // Even a hand-made plan (a link appeared after planning) writes nothing outside.
        let forced = Plan {
            changes: vec![FileChange {
                path: Kind::Codex.config_path(),
                old: None,
                new: "x = 1\n".into(),
            }],
            problems: vec![],
        };
        let done = apply(&p, &forced, &mut |_| true, &mut |_| {}).unwrap();
        assert!(
            done.written.is_empty() && done.failed.len() == 1,
            "{done:?}"
        );
        assert!(
            files_under(&outside).is_empty(),
            "wrote outside: {:?}",
            files_under(&outside)
        );
    }

    #[test]
    fn a_linked_instruction_file_is_neither_read_nor_replaced() {
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET-CONTENT\n").unwrap();
        skip_unless!(link_file(&p.join("CLAUDE.md"), &secret), "linked CLAUDE.md");

        let plan = plan_files(&p, &[Kind::Claude], "r");
        let shown: String = plan.changes.iter().map(FileChange::diff).collect();
        assert!(
            !shown.contains("TOP-SECRET"),
            "link target leaked into a diff"
        );
        assert!(
            plan.problems
                .iter()
                .any(|(f, why)| f == Path::new("CLAUDE.md") && why.contains("link"))
        );
        let forced = Plan {
            changes: vec![FileChange {
                path: PathBuf::from("CLAUDE.md"),
                old: None,
                new: snippet(),
            }],
            problems: vec![],
        };
        let done = apply(&p, &forced, &mut |_| true, &mut |_| {}).unwrap();
        assert!(done.written.is_empty(), "{done:?}");
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "TOP-SECRET-CONTENT\n"
        );
    }

    #[test]
    fn a_planted_temp_name_link_cannot_truncate_its_target() {
        // The old code wrote `<file>.herdmesh-tmp` with fs::write, which follows a link.
        let t = tmp();
        let (p, outside) = (t.path().join("proj"), t.path().join("outside"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        for name in ["CLAUDE.herdmesh-tmp", ".CLAUDE.md.herdmesh-tmp"] {
            skip_unless!(link_file(&p.join(name), &victim), "planted temp link");
        }
        let plan = plan_files(&p, &[Kind::Claude], "r");
        let done = apply(&p, &plan, &mut |_| true, &mut |_| {}).unwrap();
        assert!(done.written.iter().any(|f| f == Path::new("CLAUDE.md")));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    #[test]
    fn writes_leave_no_temp_files_and_are_still_idempotent() {
        let t = tmp();
        let p = t.path();
        let plan = plan_files(p, &Kind::ALL, "r");
        let done = all(&plan, p);
        assert!(done.failed.is_empty(), "{done:?}");
        assert!(
            files_under(p)
                .iter()
                .all(|f| !f.to_string_lossy().contains("herdmesh-tmp"))
        );
        assert_eq!(plan_files(p, &Kind::ALL, "r"), Plan::default());
    }

    #[test]
    fn root_given_through_a_link_is_resolved_and_contained() {
        let t = tmp();
        let (real, via) = (t.path().join("real"), t.path().join("via"));
        std::fs::create_dir_all(&real).unwrap();
        skip_unless!(link_dir(&via, &real), "linked project root");
        let plan = plan_files(&via, &[Kind::Claude], "r");
        assert!(plan.problems.is_empty(), "{:?}", plan.problems);
        all(&plan, &via);
        assert!(real.join(".mcp.json").is_file());
    }

    #[test]
    fn unsafe_relative_paths_are_refused() {
        let t = tmp();
        let root = std::fs::canonicalize(t.path()).unwrap();
        for bad in ["../x", "/abs", "a/../../x", ""] {
            assert!(checked_target(&root, Path::new(bad)).is_err(), "{bad}");
        }
        assert!(checked_target(&root, Path::new("ok/new.txt")).is_ok());
    }

    // ---- GATE-4 N14: identity, not basename ----

    #[test]
    fn same_named_folders_get_distinct_room_names() {
        let id = hm_proto::RoomId::from_bytes([1; 32]);
        let existing = vec![(
            id,
            crate::data::RoomConfig::new("app", crate::data::RoleName::Owner, "a-cli"),
        )];
        let (k1, k2) = (
            project::project_key(Path::new("/one/app")),
            project::project_key(Path::new("/two/app")),
        );
        assert_eq!(unique_room_name("app", &[], &k1).unwrap(), "app");
        let alt = unique_room_name("app", &existing, &k1).unwrap();
        assert_eq!(alt, format!("app-{}", &k1[..8]));
        assert_ne!(alt, unique_room_name("app", &existing, &k2).unwrap());
        assert!(hm_proto::names::valid_name(&alt));
        let long = "x".repeat(32);
        let existing_long = vec![(
            id,
            crate::data::RoomConfig::new(&long, crate::data::RoleName::Owner, "a-cli"),
        )];
        assert!(hm_proto::names::valid_name(
            &unique_room_name(&long, &existing_long, &k1).unwrap()
        ));
        // The derived name is also taken: refuse instead of guessing.
        let both = vec![
            existing[0].clone(),
            (
                hm_proto::RoomId::from_bytes([2; 32]),
                crate::data::RoomConfig::new(&alt, crate::data::RoleName::Owner, "a-cli"),
            ),
        ];
        assert!(unique_room_name("app", &both, &k1).is_err());
    }
}
