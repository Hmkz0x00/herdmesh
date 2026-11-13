//! Best-effort system clipboard through the platform's own tools.
//!
//! No new dependency: Windows uses `clip.exe` / PowerShell `Get-Clipboard`
//! (hidden window), macOS `pbcopy` / `pbpaste`, Linux `wl-copy` / `xclip` /
//! `xsel` when installed. If nothing works the caller just prints the text.

use std::io::{self, Read as _, Write as _};
use std::process::{Command, Stdio};

/// One external clipboard program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tool {
    pub prog: &'static str,
    pub args: &'static [&'static str],
}

#[cfg(windows)]
pub const COPY_TOOLS: &[Tool] = &[Tool {
    prog: "clip.exe",
    args: &[],
}];
#[cfg(windows)]
pub const PASTE_TOOLS: &[Tool] = &[Tool {
    prog: "powershell.exe",
    args: &["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard"],
}];

#[cfg(target_os = "macos")]
pub const COPY_TOOLS: &[Tool] = &[Tool {
    prog: "pbcopy",
    args: &[],
}];
#[cfg(target_os = "macos")]
pub const PASTE_TOOLS: &[Tool] = &[Tool {
    prog: "pbpaste",
    args: &[],
}];

#[cfg(not(any(windows, target_os = "macos")))]
pub const COPY_TOOLS: &[Tool] = &[
    Tool {
        prog: "wl-copy",
        args: &[],
    },
    Tool {
        prog: "xclip",
        args: &["-selection", "clipboard"],
    },
    Tool {
        prog: "xsel",
        args: &["--clipboard", "--input"],
    },
];
#[cfg(not(any(windows, target_os = "macos")))]
pub const PASTE_TOOLS: &[Tool] = &[
    Tool {
        prog: "wl-paste",
        args: &["--no-newline"],
    },
    Tool {
        prog: "xclip",
        args: &["-selection", "clipboard", "-o"],
    },
    Tool {
        prog: "xsel",
        args: &["--clipboard", "--output"],
    },
];

fn command(tool: &Tool) -> Command {
    let mut c = Command::new(tool.prog);
    c.args(tool.args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Feed `text` to the tool's stdin. `Ok(false)` = tool missing or failed.
fn run_copy(tool: &Tool, text: &str) -> io::Result<bool> {
    let mut child = match command(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    Ok(child.wait()?.success())
}

/// Read the tool's stdout. `Ok(None)` = tool missing or failed.
fn run_paste(tool: &Tool) -> io::Result<Option<String>> {
    let mut child = match command(tool)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        // Clipboards are small; cap anyway.
        stdout.by_ref().take(1 << 20).read_to_string(&mut out)?;
    }
    Ok(child.wait()?.success().then_some(out))
}

/// Try each tool until one accepts `text`; returns the program that did.
pub fn copy_with(
    tools: &[Tool],
    text: &str,
    mut run: impl FnMut(&Tool, &str) -> io::Result<bool>,
) -> Option<&'static str> {
    tools
        .iter()
        .find(|t| run(t, text).unwrap_or(false))
        .map(|t| t.prog)
}

/// Try each tool until one returns clipboard text.
pub fn paste_with(
    tools: &[Tool],
    mut run: impl FnMut(&Tool) -> io::Result<Option<String>>,
) -> Option<String> {
    tools.iter().find_map(|t| {
        run(t)
            .ok()
            .flatten()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    })
}

/// Copy `text` to the system clipboard; `None` when no tool is available.
pub fn copy(text: &str) -> Option<&'static str> {
    copy_with(COPY_TOOLS, text, run_copy)
}

/// Current clipboard text (trimmed), if any tool can read it.
pub fn paste() -> Option<String> {
    paste_with(PASTE_TOOLS, run_paste)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Tool = Tool {
        prog: "a",
        args: &[],
    };
    const B: Tool = Tool {
        prog: "b",
        args: &[],
    };

    #[test]
    fn copy_uses_the_first_tool_that_works() {
        let mut seen = Vec::new();
        let used = copy_with(&[A, B], "t", |t, text| {
            seen.push((t.prog, text.to_owned()));
            Ok(t.prog == "b")
        });
        assert_eq!(used, Some("b"));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1], ("b", "t".to_owned()));
    }

    #[test]
    fn copy_survives_errors_and_reports_none_when_nothing_works() {
        let used = copy_with(&[A, B], "t", |t, _| {
            if t.prog == "a" {
                Err(io::Error::other("boom"))
            } else {
                Ok(false)
            }
        });
        assert_eq!(used, None);
        assert_eq!(copy_with(&[], "t", |_, _| Ok(true)), None);
    }

    #[test]
    fn paste_trims_and_skips_empty_or_failed_tools() {
        let got = paste_with(&[A, B], |t| {
            Ok(if t.prog == "a" {
                Some("  \n".into())
            } else {
                Some("  hm1abc \r\n".into())
            })
        });
        assert_eq!(got.as_deref(), Some("hm1abc"));
        assert_eq!(paste_with(&[A], |_| Ok(None)), None);
        assert_eq!(paste_with(&[A], |_| Ok(Some(String::new()))), None);
    }
}
