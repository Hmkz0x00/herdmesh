HANDOFF M0-3 DONE
BRANCH: impl2/m0-ci @ 8ca4588
CHANGED: .github/workflows/ci.yml (+42/-0)
TESTS: yaml parse ok; inputs checked vs upstream action.yml — `just check`: n/a
DECISIONS: checkout@v7, rust-toolchain@stable, rust-cache@v2, install-action@v2
RISKS: actionlint not run (docker down); first green needs justfile from impl1
NEXT: run actionlint locally; confirm windows job picks up powershell shell
