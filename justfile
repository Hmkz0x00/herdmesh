set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Format all crates in place.
fmt:
    cargo fmt --all

# Fail if anything is not formatted.
fmt-check:
    cargo fmt --all -- --check

# Lint every target; warnings are errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run all tests.
test:
    cargo test --workspace

# Gate before handoff: fmt --check + clippy + test.
check: fmt-check clippy test
