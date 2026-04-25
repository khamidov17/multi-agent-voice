# Nova PR Queue — seed tasks

12 small, real, scoped tasks for Nova to work through after Phase 4 enables.
Mix of mechanical (auto-merge candidates) and semantic (manual review) so
the classifier gets exercised across categories during shadow-mode and the
30-day kill-criterion window.

Format per task: title, why, acceptance, blast radius, expected Phase 4
verdict (FMT_DRIFT, DEAD_CODE_EXTRA_LINES, OK, manual).

---

## Pure mechanical — auto-merge candidates (Phase 4.0)

### #1 — `cargo fmt --all` across the repo

**Why:** Ground-truth fmt drift removal. Establishes the corpus baseline
for the classifier (the Assignment in `tools/classify-pr/fixtures/`).

**Acceptance:** `cargo fmt --all --check` exits 0 after the PR. Diff is
byte-identical to what `cargo fmt --all` produces. No semantic changes.

**Blast radius:** Whitespace / line wraps only. Compiler output unchanged.

**Expected verdict:** `OK` (eligible) under Phase 4.0. **First test PR for
the classifier.** If the classifier rejects this, the classifier is wrong.

### #2 — Drop "Phase 0" prefix from `protected_write` tool description

**Why:** TODOS.md "Phase 0 — deferred from /review on 2026-04-21 → Maintainability"
flagged this as stale. Phase 0 is shipped; the prefix in
`src/chatbot/tools.rs`'s `protected_write` description is now noise.

**Acceptance:** Tool description reads as a present-tense description of
what the tool does, no "(Phase 0)" prefix. Behavior unchanged.

**Blast radius:** ~3 line edit in `tools.rs`. No tests break.

**Expected verdict:** `FMT_DRIFT` (cosmetic string change is NOT
fmt-equivalent) — manual merge.

### #3 — `cargo clippy --fix` for trivially-removable unused imports

**Why:** Some files accumulated unused imports across phases. Mechanical
removal via clippy's auto-fix.

**Acceptance:** `cargo clippy --all-targets -- -W unused_imports` reports
zero unused imports in the diff. `cargo test` still passes.

**Blast radius:** Imports only.

**Expected verdict:** `DEAD_CODE_EXTRA_LINES` — manual merge in 4.0,
auto-merge eligible in **Phase 4.1** when dead-code classifier ships.

---

## Tests — manual review, bounded scope

### #4 — Unit tests for `execute_protected_write` gates

**Why:** TODOS.md "Phase 0 — deferred from /review → Testing." Currently
zero coverage on the new dispatch module's gates.

**Acceptance:** New test cases cover: Tier-2 rejection, guardian-absent,
empty path, relative path, empty reason, oversized content. Run via
`cargo test execute_protected_write_`.

**Blast radius:** Test file additions only.

**Expected verdict:** `manual` — this is genuine new logic to review.

### #5 — Unit tests for `load_key`

**Why:** TODOS.md "Phase 0 — deferred from /review → Testing." Currently
no test that mode != 0400/0600 is rejected, or that a key < 32 bytes is
rejected.

**Acceptance:** Test covers (a) mode 0644 rejected, (b) mode 0600
accepted, (c) 16-byte key rejected, (d) 32-byte key accepted. Tempfile-based.

**Blast radius:** Test additions in `bootstrap-guardian/src/`.

**Expected verdict:** `manual`.

### #6 — Unit tests for `sweep_old_logs` retention

**Why:** TODOS.md "Phase 0 — deferred from /review → Testing." `file-rotate`
takes care of size-cap rotation but the retention sweep wasn't tested.

**Acceptance:** Test creates N synthetic rotated files with ages >168 days
and < 168 days; assert old ones are removed, recent ones are kept.

**Blast radius:** Test additions.

**Expected verdict:** `manual`.

### #7 — Unit test: `journal::emit` survives poisoned mutex

**Why:** TODOS.md "Phase 0 — deferred from /review → Testing." Poisoned
mutex must be swallowed (logged via `tracing::warn!`), not panic.

**Acceptance:** Test forces a poisoned mutex, calls `journal::emit`,
asserts no panic and a `warn!` was emitted (capturing via `tracing-test`).

**Blast radius:** Test additions; no production code change.

**Expected verdict:** `manual`.

---

## Docs — manual review, low-risk

### #8 — README.md: add Phase 4 section linking to docs

**Why:** README currently doesn't mention Phase 4. Newcomers landing on the
repo see "Trio Telegram bot" but no signpost to the auto-merge pipeline.

**Acceptance:** ~10-line section in README under a "Phase 4 — auto-merge"
heading, links to `docs/phase4-setup.md` / `runbook.md` / `debugging.md`.

**Blast radius:** README only.

**Expected verdict:** `manual` (touches `.md` outside fmt-classifier scope).

### #9 — CLAUDE.md: collapse the "Phase 0 (COMPLETE)" intro into the roadmap

**Why:** CLAUDE.md currently has both a long Phase-0-specific intro AND a
roadmap table that already covers Phase 0's status. Redundant. Trim the
intro to a 3-line summary; the roadmap table is the source of truth.

**Acceptance:** CLAUDE.md is ~50 lines shorter. No information loss — the
intro's claims are already captured by the roadmap, deferred TODOs.md
items, and the bootstrap-guardian doc.

**Blast radius:** CLAUDE.md only.

**Expected verdict:** `manual`.

---

## Refactors — manual review, larger

### #10 — Split `execute_protected_write` into validate / dispatch / journal_outcome

**Why:** TODOS.md "Phase 0 — deferred from /review → Maintainability."
Current ~180-line function interleaves hot path with failure paths.

**Acceptance:** Three private fns: `validate_request`, `dispatch_rpc`,
`journal_outcome`. Public `execute_protected_write` is now an orchestrator
~30 lines. All existing tests pass without changes.

**Blast radius:** `src/chatbot/tool_dispatch/protected_write.rs` only.

**Expected verdict:** `manual` (semantic refactor).

### #11 — Introduce `ClaudeSpawnConfig` struct

**Why:** TODOS.md "Phase 0 — deferred from /review → Maintainability."
`setup_claude_process` takes 9 parameters behind
`#[allow(clippy::too_many_arguments)]`.

**Acceptance:** New `ClaudeSpawnConfig` struct holds the 9 fields. Function
signature drops to `setup_claude_process(cfg: &ClaudeSpawnConfig)`.
Allow-attribute removed. Call sites updated.

**Blast radius:** `src/chatbot/claude_code.rs` + 2-3 call sites.

**Expected verdict:** `manual`.

### #12 — Group `ChatbotConfig` fields into sub-structs

**Why:** TODOS.md "Phase 0 — deferred from /review → Maintainability."
`ChatbotConfig` has 22+ fields and god-object risk grows with each phase.

**Acceptance:** Group into `PermissionsConfig`, `CognitiveConfig`,
`GuardianConfig`, `ObservabilityConfig`. JSON loader still accepts the
flat shape via `#[serde(flatten)]`. Existing tests pass.

**Blast radius:** `src/config.rs` + ~15 call sites doing
`config.field` → `config.permissions.field` etc.

**Expected verdict:** `manual` (largest task).

---

## Sequencing recommendation

**Phase 4 shadow-mode warm-up (week 1):**
- #1 (fmt-only) is the first PR. If the classifier emits `OK` on this, the
  pipeline works end-to-end. If not, the classifier has a bug — fix before
  proceeding.
- #2 (string change) is the first FMT_DRIFT verdict. Confirms the
  classifier correctly rejects non-fmt diffs.
- #3 (unused imports) is the first DEAD_CODE-shaped diff. Will be
  manual-merged in 4.0 but exercises the corpus for 4.1.

**Test backfill (week 2):**
- #4, #5, #6, #7. All manual-merged. Improves coverage from the deferred
  items in TODOS.md. Each is one file, ≤200 lines.

**Doc cleanup (week 2):**
- #8, #9. Manual-merged. Aligns the docs with shipped reality.

**Refactor backlog (week 3+):**
- #10, #11, #12. Larger but bounded. Each delivers maintainability
  improvements explicitly tracked in TODOS.md as "deferred from /review."

## How to feed these to Nova

Each task title + acceptance criterion is enough for Nova's Phase 2
fix-plan stage. Open them as GitHub issues with the `nova:queue` label
and Nova's existing draft loop will pick them up, draft a fix plan, ping
you for approval, then auto-implement via the Phase 3 worktree flow.

Recommended: open #1, watch it land via Phase 4 auto-merge, then open
#2-3 to exercise the rejection paths, THEN open the remaining 9 once the
classifier is trusted.
