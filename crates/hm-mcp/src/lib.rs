//! Stdio MCP boundary. Backends own identity, authorization, redaction and persistence.
//! All returned text is peer-authored and is wrapped here, including author labels.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod daemon_backend;
pub use daemon_backend::DaemonBackend;

use async_trait::async_trait;
use hm_proto::budget::{BRIEF_DEFAULT_MAX, RATIONALE_MAX, ROOM_CARD_MAX, estimate_tokens};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext};
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
const TEXT_MAX: usize = 8192;
const ITEM_MAX: usize = 64;
const TTL_MAX_MIN: u32 = 480;

/// Backend failures are deliberately opaque at the MCP boundary: raw errors may contain peer text.
#[derive(Debug, thiserror::Error)]
#[error("backend unavailable")]
pub struct BackendError;

/// Local adapter contract, not a wire protocol. Each result is (person/agent, text).
/// The adapter must supply verified provenance, enforce roles, redact outbound secrets,
/// and implement claim settling (pending/granted/lost). Never return pre-wrapped text.
/// `inbox(false)` reads without acknowledging; `inbox(true)` acknowledges returned items.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn room_card(&self) -> Result<Vec<(String, String)>, BackendError>;
    async fn brief(&self, max_tokens: usize) -> Result<Vec<(String, String)>, BackendError>;
    async fn inbox(&self, ack: bool) -> Result<Vec<(String, String)>, BackendError>;
    async fn ask(
        &self,
        to: String,
        question: String,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn answer(
        &self,
        ask_id: String,
        text: String,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn handoff(
        &self,
        to: Option<String>,
        summary: String,
        refs: Vec<String>,
        next_steps: Vec<String>,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn claim(
        &self,
        paths: Vec<String>,
        ttl_min: u32,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn release(&self, paths: Vec<String>) -> Result<Vec<(String, String)>, BackendError>;
    async fn task_add(
        &self,
        task: String,
        title: String,
        paths: Vec<String>,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn task_update(
        &self,
        task: String,
        status: String,
        note: Option<String>,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn task_list(
        &self,
        filter: Option<String>,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn decide(
        &self,
        title: String,
        rationale: String,
        refs: Vec<String>,
    ) -> Result<Vec<(String, String)>, BackendError>;
    async fn get_event(&self, id: String) -> Result<Vec<(String, String)>, BackendError>;
    async fn search(
        &self,
        query: String,
        limit: usize,
    ) -> Result<Vec<(String, String)>, BackendError>;
}

fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() && c != '\n' && c != '\t' => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}
/// Escape before delimiting; neither the label nor body can forge another block.
pub fn wrap_peer(author: &str, text: &str) -> String {
    format!(
        "<<peer-untrusted from=\"{}\">>{}<</peer-untrusted>>",
        escape(author),
        escape(text)
    )
}

// A single descriptor drives both advertisement and unknown/required-field validation.
// ? denotes optional. Types/lengths are enforced by the dispatcher below.
const SPECS: &[(&str, &str, &str)] = &[
    ("room_card", "Room", ""),
    ("brief", "Brief", "max_tokens?"),
    ("inbox", "Inbox", "ack?"),
    ("ask", "Ask", "to question"),
    ("answer", "Answer", "ask_id text"),
    ("handoff", "Handoff", "to? summary refs next_steps"),
    ("claim", "Lease", "paths ttl_min"),
    ("release", "Release", "paths"),
    ("task_add", "Add", "task title paths"),
    ("task_update", "Update", "task status note?"),
    ("task_list", "Tasks", "filter?"),
    ("decide", "Decide", "title rationale refs"),
    ("get_event", "Event", "id"),
    ("search", "Search", "query limit"),
];
/// Complete, stable tools/list response. Schemas omit redundant titles and descriptions.
pub fn tools_list() -> ListToolsResult {
    let tools = SPECS
        .iter()
        .map(|(name, description, fields)| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields.split_whitespace() {
                let key = field.trim_end_matches('?');
                let schema = match key {
                    "paths" | "refs" | "next_steps" => {
                        json!({"type":"array", "items":{"type":"string"}})
                    }
                    "max_tokens" | "ttl_min" | "limit" => json!({"type":"integer"}),
                    "ack" => json!({"type":"boolean"}),
                    _ => json!({}),
                };
                properties.insert(key.into(), schema);
                if !field.ends_with('?') {
                    required.push(key);
                }
            }
            let mut schema = Map::new();
            schema.insert("type".into(), json!("object"));
            if !properties.is_empty() {
                schema.insert("properties".into(), Value::Object(properties));
            }
            if !required.is_empty() {
                schema.insert("required".into(), json!(required));
            }
            Tool::new(*name, *description, schema)
        })
        .collect();
    ListToolsResult {
        tools,
        ..Default::default()
    }
}

fn invalid() -> ErrorData {
    ErrorData::invalid_params("Invalid arguments", None)
}
fn text(args: &Map<String, Value>, key: &str, max: usize) -> Result<String, ErrorData> {
    let s = args.get(key).and_then(Value::as_str).ok_or_else(invalid)?;
    if s.trim().is_empty()
        || s.len() > max
        || s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(invalid());
    }
    Ok(s.into())
}
fn optional(args: &Map<String, Value>, key: &str, max: usize) -> Result<Option<String>, ErrorData> {
    if args.contains_key(key) {
        text(args, key, max).map(Some)
    } else {
        Ok(None)
    }
}
fn number(
    args: &Map<String, Value>,
    key: &str,
    default: Option<usize>,
    max: usize,
) -> Result<usize, ErrorData> {
    let n = match args.get(key) {
        Some(v) => v
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(invalid)?,
        None => default.ok_or_else(invalid)?,
    };
    if n == 0 || n > max {
        Err(invalid())
    } else {
        Ok(n)
    }
}
fn strings(args: &Map<String, Value>, key: &str, max: usize) -> Result<Vec<String>, ErrorData> {
    let values = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if values.len() > ITEM_MAX {
        return Err(invalid());
    }
    values
        .iter()
        .map(|v| {
            let a = Map::from_iter([(key.into(), v.clone())]);
            text(&a, key, max)
        })
        .collect()
}
fn paths(args: &Map<String, Value>, nonempty: bool) -> Result<Vec<String>, ErrorData> {
    let paths = strings(args, "paths", 1024)?;
    if nonempty && paths.is_empty() {
        return Err(invalid());
    }
    for p in &paths {
        if p.starts_with(['/', '\\'])
            || p.contains(':')
            || p.chars().any(char::is_control)
            || p.split(['/', '\\'])
                .any(|part| part == ".." || part == "." || part.is_empty())
        {
            return Err(invalid());
        }
    }
    Ok(paths)
}
fn event(args: &Map<String, Value>, key: &str) -> Result<String, ErrorData> {
    let id = text(args, key, 64)?;
    hm_proto::EventId::from_hex(&id).ok_or_else(invalid)?;
    Ok(id)
}
fn task(args: &Map<String, Value>) -> Result<String, ErrorData> {
    let id = text(args, "task", 64)?;
    hm_proto::TaskId::new(&id).map_err(|_| invalid())?;
    Ok(id)
}
fn refs(args: &Map<String, Value>) -> Result<Vec<String>, ErrorData> {
    let refs = strings(args, "refs", 1024)?;
    for r in &refs {
        r.parse::<hm_proto::Ref>().map_err(|_| invalid())?;
    }
    Ok(refs)
}

#[derive(Clone)]
pub struct McpServer {
    backend: Arc<dyn Backend>,
}
impl McpServer {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }
    /// Dispatch only after validating arguments. No peer text is interpreted as instructions.
    pub async fn invoke(&self, name: &str, args: Value) -> Result<String, ErrorData> {
        let (_, _, fields) = SPECS
            .iter()
            .find(|s| s.0 == name)
            .ok_or_else(ErrorData::method_not_found::<rmcp::model::CallToolRequestMethod>)?;
        let args = args.as_object().ok_or_else(invalid)?;
        if args.keys().any(|k| {
            !fields
                .split_whitespace()
                .any(|f| f.trim_end_matches('?') == k)
        }) || fields
            .split_whitespace()
            .any(|f| !f.ends_with('?') && !args.contains_key(f))
        {
            return Err(invalid());
        }
        let b = &self.backend;
        let result = match name {
            "room_card" => b.room_card().await,
            "brief" => {
                b.brief(number(
                    args,
                    "max_tokens",
                    Some(BRIEF_DEFAULT_MAX),
                    usize::MAX,
                )?)
                .await
            }
            "inbox" => {
                let ack = match args.get("ack") {
                    None => false,
                    Some(v) => v.as_bool().ok_or_else(invalid)?,
                };
                b.inbox(ack).await
            }
            "ask" => {
                b.ask(text(args, "to", 256)?, text(args, "question", TEXT_MAX)?)
                    .await
            }
            "answer" => {
                b.answer(event(args, "ask_id")?, text(args, "text", TEXT_MAX)?)
                    .await
            }
            "handoff" => {
                b.handoff(
                    optional(args, "to", 256)?,
                    text(args, "summary", TEXT_MAX)?,
                    refs(args)?,
                    strings(args, "next_steps", TEXT_MAX)?,
                )
                .await
            }
            "claim" => {
                b.claim(
                    paths(args, true)?,
                    number(args, "ttl_min", None, TTL_MAX_MIN as usize)? as u32,
                )
                .await
            }
            "release" => b.release(paths(args, true)?).await,
            "task_add" => {
                b.task_add(task(args)?, text(args, "title", 256)?, paths(args, false)?)
                    .await
            }
            "task_update" => {
                let status = text(args, "status", 32)?;
                if !["open", "in_progress", "blocked", "done", "dropped"].contains(&status.as_str())
                {
                    return Err(invalid());
                }
                b.task_update(task(args)?, status, optional(args, "note", TEXT_MAX)?)
                    .await
            }
            "task_list" => b.task_list(optional(args, "filter", 256)?).await,
            "decide" => {
                let rationale = text(args, "rationale", TEXT_MAX)?;
                if estimate_tokens(&rationale) > RATIONALE_MAX {
                    return Err(invalid());
                }
                b.decide(text(args, "title", 256)?, rationale, refs(args)?)
                    .await
            }
            "get_event" => b.get_event(event(args, "id")?).await,
            "search" => {
                b.search(
                    text(args, "query", 1024)?,
                    number(args, "limit", None, 100)?,
                )
                .await
            }
            _ => return Err(invalid()),
        }
        .map_err(|_| ErrorData::internal_error("Backend unavailable", None))?;
        let cap = match name {
            "room_card" => Some(ROOM_CARD_MAX),
            "brief" => Some(number(
                args,
                "max_tokens",
                Some(BRIEF_DEFAULT_MAX),
                usize::MAX,
            )?),
            _ => None,
        };
        // Backends order brief/card items by relevance, highest first. Drop whole
        // trailing items, never truncate a peer block or its closing delimiter.
        let mut output = String::new();
        for (author, text) in result {
            let block = wrap_peer(&author, &text);
            let candidate = if output.is_empty() {
                block
            } else {
                format!("{output}\n{block}")
            };
            if cap.is_some_and(|max| estimate_tokens(&candidate) > max) {
                break;
            }
            output = candidate;
        }
        Ok(output)
    }
}
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(tools_list())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if !SPECS.iter().any(|spec| spec.0 == request.name) {
            return Err(ErrorData::method_not_found::<CallToolRequestMethod>());
        }
        let result = match self
            .invoke(
                &request.name,
                Value::Object(request.arguments.unwrap_or_default()),
            )
            .await
        {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error.message)]),
        };
        Ok(result.into())
    }
}
/// Run one authenticated local-agent adapter over stdin/stdout until disconnected.
/// Logging must use stderr; the transport owns stdout.
pub async fn serve_stdio(
    backend: Arc<dyn Backend>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    McpServer::new(backend)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
