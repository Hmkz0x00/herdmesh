#!/usr/bin/env bash
# herdmesh dev-environment bootstrap. Run from the repo root.
# Linux/macOS, or WSL2 on Windows (recommended: herdr's native Windows support is beta).
#   scripts/bootstrap.sh          install/verify everything
#   scripts/bootstrap.sh --check  only verify
set -uo pipefail
CHECK_ONLY=${1:-}
ok(){ printf '  \033[32m✔\033[0m %s\n' "$1"; }
bad(){ printf '  \033[31m✘\033[0m %s\n' "$1"; MISSING=1; }
MISSING=0
has(){ command -v "$1" >/dev/null 2>&1; }

echo "== Tools"
for t in herdr claude codex agy cargo rust-analyzer just git node; do
  if has "$t"; then ok "$t"; else bad "$t not found"; fi
done

if [[ "$CHECK_ONLY" == "--check" ]]; then
  [[ -n "${CONTEXT7_API_KEY:-}" ]] && ok "CONTEXT7_API_KEY set" || bad "CONTEXT7_API_KEY not set (free key at context7.com)"
  exit $MISSING
fi

echo "== Rust toolchain extras"
has rustup && rustup component add rust-analyzer clippy rustfmt || bad "rustup missing: https://rustup.rs"
has just || cargo install just

echo "== Shared skills (herdr skill → all agents, project scope)"
if has npx; then
  npx -y skills add herdrdev/herdr --skill herdr || bad "herdr skill install failed"
else
  bad "npx missing. Alternative: run 'herdr --skill > .agents/skills/herdr/SKILL.md'"
fi

echo "== Mirror project skills into Claude's skills dir"
mkdir -p .claude/skills
for s in .agents/skills/*/; do
  name=$(basename "$s"); mkdir -p ".claude/skills/$name"; cp -r "$s"* ".claude/skills/$name/"
done
ok "skills mirrored"

echo "== Claude Code plugins (project scope → written to .claude/settings.json)"
if has claude; then
  claude plugin marketplace add obra/superpowers-marketplace || true
  for p in superpowers@superpowers-marketplace \
           rust-analyzer-lsp@claude-plugins-official \
           security-guidance@claude-plugins-official \
           commit-commands@claude-plugins-official \
           pr-review-toolkit@claude-plugins-official; do
    claude plugin install "$p" --scope project || bad "plugin $p"
  done
fi

echo "== Antigravity CLI: reuse the Claude setup (optional)"
has agy && { agy plugin import claude || true; }

echo "== Context7 key"
[[ -n "${CONTEXT7_API_KEY:-}" ]] && ok "CONTEXT7_API_KEY set" \
  || bad "export CONTEXT7_API_KEY=... and put it in .codex/config.toml + .agents/mcp_config.json"

echo
[[ $MISSING -eq 0 ]] && echo "All good. Start herdr, open a Claude pane named 'lead', and paste MASTER_PROMPT.md." \
                     || echo "Fix the ✘ items above, then re-run."
exit $MISSING
