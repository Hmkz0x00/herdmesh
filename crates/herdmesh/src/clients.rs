//! Registry of agent CLIs `herdmesh setup` can configure (data, not code).
//!
//! Paths and formats come from `docs/research/mcp-clients.md`, researched from
//! vendor docs and best-effort: fix a wrong row here, nothing else changes.
//! Only project-level files are listed; user-level-only CLIs are `MANUAL`.

use std::path::PathBuf;

/// An agent CLI we can configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Claude,
    Codex,
    Agy,
    Gemini,
    Opencode,
    Cursor,
    Copilot,
    Kiro,
    Qwen,
    Amp,
    Droid,
    Kimi,
    Devin,
    Grok,
    Pi,
}

/// Where the server table lives and how the file is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JSON object; `path` are nested keys down to the servers object.
    Json(&'static [&'static str]),
    /// TOML; `path` are nested tables down to the servers table.
    Toml(&'static [&'static str]),
}

/// Shape of our server entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `{ "command": "herdmesh", "args": [...] }`
    CommandArgs,
    /// `{ "type": "local", "command": "herdmesh", "args": [...] }`
    LocalCommandArgs,
    /// `{ "type": "local", "command": ["herdmesh", ...] }`
    LocalCommandArray,
}

pub struct Client {
    pub kind: Kind,
    pub name: &'static str,
    /// Executables looked up on `PATH` (any one means installed).
    pub exes: &'static [&'static str],
    /// Project-relative config file, as path components.
    pub config: &'static [&'static str],
    pub format: Format,
    pub shape: Shape,
    pub docs: &'static str,
}

const MCP_SERVERS: Format = Format::Json(&["mcpServers"]);

pub const REGISTRY: [Client; 15] = [
    Client {
        kind: Kind::Claude,
        name: "claude",
        exes: &["claude"],
        config: &[".mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://docs.claude.com/en/docs/claude-code/mcp",
    },
    Client {
        kind: Kind::Codex,
        name: "codex",
        exes: &["codex"],
        config: &[".codex", "config.toml"],
        format: Format::Toml(&["mcp_servers"]),
        shape: Shape::CommandArgs,
        docs: "https://developers.openai.com/codex/mcp",
    },
    Client {
        kind: Kind::Agy,
        name: "agy",
        exes: &["agy"],
        config: &[".agents", "mcp_config.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "docs/agents-setup.md",
    },
    Client {
        kind: Kind::Gemini,
        name: "gemini",
        exes: &["gemini"],
        config: &[".gemini", "settings.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://geminicli.com/docs/tools/mcp/",
    },
    Client {
        kind: Kind::Opencode,
        name: "opencode",
        exes: &["opencode"],
        config: &["opencode.json"],
        // opencode keeps servers directly under "mcp" (research note listed
        // mcp/servers; the vendor schema has no "servers" level).
        format: Format::Json(&["mcp"]),
        shape: Shape::LocalCommandArray,
        docs: "https://opencode.ai/docs/mcp/",
    },
    Client {
        kind: Kind::Cursor,
        name: "cursor",
        exes: &["agent"],
        config: &[".cursor", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://docs.cursor.com/context/model-context-protocol",
    },
    Client {
        kind: Kind::Copilot,
        name: "copilot",
        exes: &["copilot"],
        config: &[".github", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::LocalCommandArgs,
        docs: "https://docs.github.com/en/copilot/using-github-copilot/using-copilot-in-the-cli",
    },
    Client {
        kind: Kind::Kiro,
        name: "kiro",
        exes: &["kiro-cli"],
        config: &[".kiro", "settings", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://kiro.dev/docs/mcp/",
    },
    Client {
        kind: Kind::Qwen,
        name: "qwen",
        exes: &["qwen"],
        config: &[".qwen", "settings.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://qwenlm.github.io/qwen-code/",
    },
    Client {
        kind: Kind::Amp,
        name: "amp",
        exes: &["amp"],
        config: &[".amp", "settings.json"],
        format: Format::Json(&["amp.mcpServers"]),
        shape: Shape::CommandArgs,
        docs: "https://ampcode.com/docs/mcp",
    },
    Client {
        kind: Kind::Droid,
        name: "droid",
        exes: &["droid"],
        config: &[".factory", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://docs.factory.ai/cli/mcp",
    },
    Client {
        kind: Kind::Kimi,
        name: "kimi",
        exes: &["kimi"],
        config: &[".kimi-code", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://code.kimi.com/docs/mcp",
    },
    Client {
        kind: Kind::Devin,
        name: "devin",
        exes: &["devin"],
        config: &[".devin", "mcp_config.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://docs.devin.ai/cli/mcp",
    },
    Client {
        kind: Kind::Grok,
        name: "grok",
        exes: &["grok"],
        config: &[".grok", "config.toml"],
        format: Format::Toml(&["mcp_servers"]),
        shape: Shape::CommandArgs,
        docs: "https://docs.x.ai/tools/mcp",
    },
    Client {
        kind: Kind::Pi,
        name: "pi",
        exes: &["pi"],
        config: &[".pi", "mcp.json"],
        format: MCP_SERVERS,
        shape: Shape::CommandArgs,
        docs: "https://pi.dev/docs/mcp/",
    },
];

/// CLIs with only user-level MCP config: never written, a hint is printed.
pub const MANUAL: [(&str, &str); 2] = [
    (
        "cline",
        "add the herdmesh server in ~/.cline/data/settings/cline_mcp_settings.json (https://cline.bot/docs/mcp)",
    ),
    (
        "goose",
        "add a stdio extension in ~/.config/goose/config.yaml (https://block.github.io/goose/docs/guides/extensions/)",
    ),
];

impl Kind {
    pub const ALL: [Kind; 15] = [
        Kind::Claude,
        Kind::Codex,
        Kind::Agy,
        Kind::Gemini,
        Kind::Opencode,
        Kind::Cursor,
        Kind::Copilot,
        Kind::Kiro,
        Kind::Qwen,
        Kind::Amp,
        Kind::Droid,
        Kind::Kimi,
        Kind::Devin,
        Kind::Grok,
        Kind::Pi,
    ];

    pub fn client(self) -> &'static Client {
        REGISTRY
            .iter()
            .find(|c| c.kind == self)
            .unwrap_or(&REGISTRY[0])
    }

    /// The `--kind` value (not always the executable name, e.g. cursor).
    pub fn name(self) -> &'static str {
        self.client().name
    }

    pub fn config_path(self) -> PathBuf {
        self.client().config.iter().collect()
    }

    pub fn is_toml(self) -> bool {
        matches!(self.client().format, Format::Toml(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_kind_once_with_safe_relative_paths() {
        for k in Kind::ALL {
            assert_eq!(REGISTRY.iter().filter(|c| c.kind == k).count(), 1, "{k:?}");
            assert_eq!(k.client().kind, k);
        }
        for c in &REGISTRY {
            assert!(!c.exes.is_empty() && !c.config.is_empty());
            assert!(
                c.config.iter().all(|p| !p.is_empty()
                    && !p.chars().any(std::path::is_separator)
                    && *p != ".."),
                "{}",
                c.name
            );
            assert!(hm_proto::names::valid_name(c.name), "{}", c.name);
        }
    }
}
