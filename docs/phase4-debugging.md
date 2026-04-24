# Phase 4 — Debugging Guide

My PR wasn't auto-merged. Why? Keyed by `reason_code` from the verdict
JSON. Every classifier workflow run attaches a `classify-verdict-pr-<N>`
artifact — start there.

## Fast path

```bash
# Check the latest verdict for your PR.
gh run list --workflow automerge-classify.yml \
  --branch "$(gh pr view <N> --json headRefName --jq .headRefName)" \
  --json databaseId,conclusion --limit 1
# Get the run id, then:
gh run download <RUN_ID> -n classify-verdict-pr-<N>
cat verdict.json | jq .
```

Or locally reproduce the classifier against the PR's diff without GitHub:

```bash
gh pr diff <N> --name-only \
  | cargo run --release -p classify-pr -- check \
      --repo-root . \
      --stdin-paths \
      --automerge-enabled 1 \
      --verbose
```

The verbose flag prints the human-readable summary to stderr.

## Reason codes

### `OK` — eligible
Classifier said yes. Status check posted `success`, label
`automerge:mechanical` applied. If the PR still hasn't merged, check:

- Are all other required CI checks green?
- Is `gh pr merge --auto` enabled for this PR? (Nova should set this when
  opening the PR; if she didn't, run `gh pr merge <N> --auto --squash`.)
- Is `AUTOMERGE_ENABLED=1`? `gh variable list | grep AUTOMERGE_ENABLED`.

### `FMT_DRIFT` — ineligible
The PR's diff is NOT byte-identical to what `cargo fmt --check` would
produce. Likely causes:

- Author edited formatting by hand (tabs vs spaces, extra blank lines,
  line-wrap not matching rustfmt).
- Author mixed fmt-only changes with a logic change.

**Fix as author:** `cargo fmt` locally, commit, push. Or split the PR:
fmt-only changes in one, logic changes in another.

The verdict's `human_message` lists the first few files where rustfmt
would change something. Workflow log has the full diff.

### `PROTECTED_PATH` — ineligible
The PR touches a path the classifier never auto-merges. See
`protected_paths.rs` for the canonical list:

- `bootstrap-guardian/**` — Phase 0 invariant.
- `.github/workflows/**` — CI is the trust boundary.
- `deploy/**` — launchd / systemd templates.
- `rust-toolchain.toml` — bumping invalidates classifier evidence.
- `tools/classify-pr/**` — the classifier cannot judge its own code.
- `supervisor/**` — Nova runs under this process.

Touching any of these is a signal that a human must review. The verdict's
`protected_paths_touched` lists the exact paths.

**Fix as author:** Split the PR. Protected-path changes go in one PR
(merged manually); mechanical changes go in another.

### `TOOLCHAIN_HASH_MISMATCH` — ineligible (Phase 4.1 only)
The classifier binary was built against a different `rust-toolchain.toml`
than the repo currently pins. Ship doesn't arrive in 4.0; 4.1 embeds the
toolchain SHA at build time and refuses to classify on drift.

**Fix as operator:** Follow the Toolchain Bump Procedure in the design
doc (ava-phase-0-design-20260424-213709.md).

### `CLIPPY_DRIFT` — ineligible (Phase 4.1 only)
Clippy lint set drifted from what the classifier was pinned against.
Same fix shape as toolchain drift.

### `PAUSED` — ineligible
`AUTOMERGE_ENABLED != "1"` when the classifier ran. Every value except
literal `"1"` is treated as paused (fail-safe; ENG-3). Check:

```bash
gh variable list | grep AUTOMERGE_ENABLED
```

- If the value is `0`: canary likely paused it. Look for an open
  `phase4-alert` issue with pause-recovery checklist.
- If the variable is **missing**: it was deleted. Recreate via
  `gh variable set AUTOMERGE_ENABLED --body 0` (start paused; then
  re-graduate shadow-mode before setting to `1`).
- If the value is something weird (`true`, `enabled`, ` 1`, `1 `):
  a hand-edit got it wrong. Fix with
  `gh variable set AUTOMERGE_ENABLED --body 1`.

### `CLASSIFIER_ERROR` — ineligible (operational)
The classifier itself failed to reach a decision. Usual causes:

- `cargo` not on the runner's PATH. Check the workflow log for the
  `Install rust toolchain from pin` step.
- `Swatinem/rust-cache` key is stale and the build failed. Force a fresh
  build by bumping the `key:` in `.github/workflows/automerge-classify.yml`.
- Classifier panicked. Workflow log has the stack trace. File an issue
  tagged `phase4-alert` and fall back to manual merge.

## Workflow did not run at all

Scenario: no `automerge/classified-safe` status check on the PR, no
workflow run in the Actions tab.

- Is the PR a draft? Draft PRs are explicitly skipped (see the `if:`
  condition in `automerge-classify.yml`).
- Is the PR from a fork? Fork PRs are skipped by policy — owner reviews
  manually.
- Did the workflow fail to parse? Check the Actions tab for a workflow
  syntax error. All YAML must pass `python3 -c "import yaml;
  yaml.safe_load(open('.github/workflows/automerge-classify.yml'))"`.

## Shadow-mode never exits

`AUTOMERGE_ENABLED` is still `0` after 2+ weeks of PR activity.

- Check `tools/classify-pr/state/shadow-decisions.jsonl`. Is it growing?
  If no rows are appending, the `shadow-mode-reconcile.yml` workflow
  isn't running — look for artifact-not-found errors.
- Count `false_positive` rows:
  ```bash
  jq -r 'select(.match == "false_positive")' tools/classify-pr/state/shadow-decisions.jsonl | wc -l
  ```
  Any non-zero = the classifier is unsafe to un-pause. Investigate the
  specific PR where the human disagreed; the classifier marked it
  eligible but the human closed it without merging.

## The JSON verdict schema

See `tools/classify-pr/src/verdict.rs` for the v1 schema. Wire-stable
fields:

- `schema` (always `1` in Phase 4.0)
- `eligible` (bool)
- `class` (`fmt-equiv` | `dead-code` | null)
- `reason_code` (enum — see above)
- `human_message`, `suggested_fix`, `docs_url`
- `alternative_action` (`split-pr` | `manual-merge` | `out-of-scope` | null)
- `protected_paths_touched` (array)
- `head_sha`, `base_sha`
- `toolchain_sha`, `clippy_lints_sha` (Phase 4.1, currently null)

Downstream consumers pin the schema version. Breaking changes bump to
schema 2 and keep old consumers on a separate field-set.
