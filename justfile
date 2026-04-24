# Trio project task runner.
#
# Install: `cargo install just` or `brew install just`.

default:
    @just --list

# Phase 4 — mechanical auto-merge classifier.

# Build the classifier in release mode (matches CI).
classify-pr-build:
    cargo build --release -p classify-pr --manifest-path tools/classify-pr/Cargo.toml

# Unit tests for the classifier.
classify-pr-test:
    cargo test --manifest-path tools/classify-pr/Cargo.toml

# Lint the classifier.
classify-pr-lint:
    cargo clippy --manifest-path tools/classify-pr/Cargo.toml -- -D warnings
    cargo fmt --manifest-path tools/classify-pr/Cargo.toml --check

# Dry-run the classifier against a local PR diff.
# Usage: `just classify-pr-dry-run 42`
classify-pr-dry-run PR:
    gh pr diff {{PR}} --name-only | \
      cargo run --release -p classify-pr \
        --manifest-path tools/classify-pr/Cargo.toml -- \
        check \
        --repo-root . \
        --stdin-paths \
        --head-sha "$(gh pr view {{PR}} --json headRefOid --jq .headRefOid)" \
        --base-sha "$(gh pr view {{PR}} --json baseRefOid --jq .baseRefOid)" \
        --automerge-enabled 1 \
        --verbose

# Pause / resume auto-merge (owner only).
automerge-pause:
    gh variable set AUTOMERGE_ENABLED --body 0
    @echo "Auto-merge paused. Set to 1 to resume."

automerge-resume:
    gh variable set AUTOMERGE_ENABLED --body 1
    @echo "Auto-merge resumed. Next eligible PR will land automatically."

automerge-status:
    @echo -n "AUTOMERGE_ENABLED="
    @gh variable list --json name,value --jq '.[] | select(.name == "AUTOMERGE_ENABLED") | .value' || echo "(unset)"
    @echo "Shadow-mode log: $$(wc -l < tools/classify-pr/state/shadow-decisions.jsonl 2>/dev/null || echo 0) decisions"

# Replay historical corpus against the current classifier (Phase 4.1 uses this
# for hand-labeled ground truth; Phase 4.0 corpus is fmt-only).
classify-pr-corpus-replay:
    @echo "Phase 4.1 feature. Fixtures live at tools/classify-pr/fixtures/historical-corpus.jsonl."
    @echo "See tools/classify-pr/fixtures/README.md for the Assignment."
