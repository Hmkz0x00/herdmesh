# Review checklist

1. **Scope** — does the diff stay inside the lease? List any extra paths.
2. **Tests first** — is there a failing test in the history before the fix?
3. **Budgets** — every agent-facing string checked with `estimate_tokens(..) <= CAP`.
4. **Errors** — `thiserror` in libs, `anyhow` only in binaries; no `unwrap()` outside tests.
5. **Logs** — no message bodies at `info`; use `trace` and redact.
6. **Docs** — public items have a one-line doc comment; ADR updated if a decision changed.

> If any item fails, mark the card `BLOCKED` and say which one.
