//! Pointers to content: `git:<sha>:<path>#L10-40`, `task:<id>`, `event:<id>`.
//! Refs never carry inline content.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RefParseError;
use crate::ids::{EventId, TaskId};

/// Inclusive 1-based line range. `start == end` prints as `L<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRange {
    start: u32,
    end: u32,
}

impl LineRange {
    pub fn new(start: u32, end: u32) -> Result<Self, RefParseError> {
        if start == 0 || start > end {
            return Err(RefParseError::BadLines);
        }
        Ok(Self { start, end })
    }
    pub fn start(&self) -> u32 {
        self.start
    }
    pub fn end(&self) -> u32 {
        self.end
    }
}

/// A validated pointer into a git tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitRef {
    sha: String,
    path: String,
    lines: Option<LineRange>,
}

impl GitRef {
    pub fn new(
        sha: impl Into<String>,
        path: impl Into<String>,
        lines: Option<LineRange>,
    ) -> Result<Self, RefParseError> {
        let (sha, path) = (sha.into(), path.into());
        let sha_ok = (7..=64).contains(&sha.len())
            && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !sha_ok {
            return Err(RefParseError::BadSha);
        }
        if path.is_empty() || path.chars().any(|c| c == '#' || c.is_control()) {
            return Err(RefParseError::BadPath);
        }
        Ok(Self { sha, path, lines })
    }
    pub fn sha(&self) -> &str {
        &self.sha
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn lines(&self) -> Option<LineRange> {
        self.lines
    }
}

/// A pointer to a git range, a task or an event. Serialized as its string form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Ref {
    Git(GitRef),
    Task(TaskId),
    Event(EventId),
}

fn parse_u32(s: &str) -> Result<u32, RefParseError> {
    // Reject signs and empty strings, which u32::from_str partly accepts.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RefParseError::BadLines);
    }
    s.parse().map_err(|_| RefParseError::BadLines)
}

fn parse_lines(s: &str) -> Result<LineRange, RefParseError> {
    match s.split_once('-') {
        Some((a, b)) => LineRange::new(parse_u32(a)?, parse_u32(b)?),
        None => {
            let n = parse_u32(s)?;
            LineRange::new(n, n)
        }
    }
}

impl FromStr for Ref {
    type Err = RefParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(rest) = s.strip_prefix("git:") {
            let (sha, rest) = rest.split_once(':').ok_or(RefParseError::BadSha)?;
            let (path, lines) = match rest.split_once('#') {
                Some((path, frag)) => {
                    let n = frag.strip_prefix('L').ok_or(RefParseError::BadLines)?;
                    (path, Some(parse_lines(n)?))
                }
                None => (rest, None),
            };
            GitRef::new(sha, path, lines).map(Ref::Git)
        } else if let Some(id) = s.strip_prefix("task:") {
            TaskId::new(id).map(Ref::Task)
        } else if let Some(hex) = s.strip_prefix("event:") {
            EventId::from_hex(hex)
                .map(Ref::Event)
                .ok_or(RefParseError::BadEventId)
        } else {
            Err(RefParseError::UnknownScheme)
        }
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ref::Git(g) => {
                write!(f, "git:{}:{}", g.sha, g.path)?;
                match g.lines {
                    Some(l) if l.start == l.end => write!(f, "#L{}", l.start),
                    Some(l) => write!(f, "#L{}-{}", l.start, l.end),
                    None => Ok(()),
                }
            }
            Ref::Task(t) => write!(f, "task:{t}"),
            Ref::Event(e) => write!(f, "event:{e}"),
        }
    }
}

impl TryFrom<String> for Ref {
    type Error = RefParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Ref> for String {
    fn from(r: Ref) -> Self {
        r.to_string()
    }
}
