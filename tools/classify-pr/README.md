# classify-pr

Phase 4 mechanical auto-merge classifier. See the design doc
(`~/.gstack/projects/khamidov17-multi-agent-voice/ava-phase-0-design-20260424-213709.md`)
and `docs/phase4-setup.md` / `docs/phase4-runbook.md` / `docs/phase4-debugging.md`
for context.

## What it does

Given a PR's list of changed paths + a repo working copy, decides whether
the PR is safe to auto-merge by running three gates in order:

1. **Pause gate.** Reads `AUTOMERGE_ENABLED`. Only the exact string `"1"`
   un-pauses. Missing/empty/anything else = paused (fail-safe).
2. **Protected-path gate.** If the PR touches `.github/workflows/**`,
   `bootstrap-guardian/**`, `deploy/**`, `rust-toolchain.toml`, or the
   classifier itself (`tools/classify-pr/**`), ineligible.
3. **Fmt-equivalence gate.** Runs `cargo fmt --check --all` in the repo.
   Clean = eligible. Drift detected = ineligible with a summary of what
   rustfmt would have changed.

v1 is fmt-only. Dead-code removal classifier lands in Phase 4.1.

## CLI

```
classify-pr check \
  --repo-root <path> \
  --changed-paths-file <path>     # or --stdin-paths
  [--head-sha <sha>] [--base-sha <sha>] \
  [--automerge-enabled <value>] \
  [--verdict-out <path>] [--verbose]

classify-pr explain --verdict-file <path|->
```

Exit codes: `0` eligible, `1` ineligible, `2` operational error, `3`
protected path, `4` toolchain drift, `5` paused.

## Local debug loop

```bash
# Against a real PR you have locally checked out.
gh pr diff 42 --name-only \
  | cargo run --release -p classify-pr -- check \
      --repo-root . \
      --stdin-paths \
      --automerge-enabled 1 \
      --verbose
```

## Trust boundary

This crate must be built from `main`, not the PR branch, or the classifier
can be modified by the PR it is judging. The `automerge-classify.yml`
workflow handles this by using `pull_request_target` and checking out the
base ref for the `tools/classify-pr/` directory. See the ENG-D1 finding in
the Phase 4 design doc.
