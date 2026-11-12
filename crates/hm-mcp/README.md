# hm-mcp adapter contract

Call `serve_stdio(Arc<dyn Backend>)` from the CLI after selecting the local agent.
The server owns stdout; send diagnostics to stderr. `McpServer::new` also accepts
an in-memory transport through rmcp `ServiceExt::serve` for embedding/tests.

`Backend` is an asynchronous, object-safe local interface, not an IPC protocol.
Every successful method returns `(verified person/agent label, raw text)` pairs.
Split mixed-author results into separate pairs. Do not pre-wrap strings. The
server escapes labels and bodies, then wraps every pair in peer-untrusted
markers. Error details are intentionally opaque and cannot leak raw peer text.

The adapter owns identity selection, role authorization, outbound redaction,
persistence, inbox acknowledgements and lease settling. `claim` must report
pending/granted/lost per ADR-002; it must never equate event acceptance with a
granted lease. `release(paths)` must resolve the caller's matching leases.
There is no daemon connection or fake production backend in this crate.

Room-card and brief pairs must be ordered by decreasing relevance (newer,
in-scope, unfinished first). The server counts the fully escaped/wrapped result
and drops whole trailing pairs to meet ROOM_CARD_MAX or requested brief budget.
A budget too small for the first pair returns empty text; delimiters are never
truncated. Backends should produce small pairs and allow space for wrappers.

The full tools/list golden fixture is checked against TOOLS_TOTAL_MAX. To fit
600 tokens, text properties use empty schemas; they are strictly validated as
strings at dispatch. Arrays advertise string items; integers and booleans have
explicit types. Unknown keys, missing fields, nulls and wrong types fail before
backend invocation. Validation errors are MCP tool errors; unknown tools are
JSON-RPC method-not-found errors.

Limits: text 8192 UTF-8 bytes; titles/recipients/filter 256; paths/refs/query 1024;
arrays 64 entries; lease TTL 1..=480 minutes; search limit 1..=100. Brief defaults
to BRIEF_DEFAULT_MAX. IDs and refs use hm-proto parsers. Task statuses are
`open`, `in_progress`, `blocked`, `done`, `dropped`. Decision rationale uses
RATIONALE_MAX. Claim/release require nonempty relative path sets; path validation
is platform-independent, including Windows separators and drive prefixes.

rmcp is pinned to 3.9.0 (requires Rust >=1.88); the workspace MSRV declaration
is owned by the lead. Integration should regenerate the workspace lockfile.
