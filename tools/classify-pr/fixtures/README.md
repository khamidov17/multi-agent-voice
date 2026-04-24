# classify-pr fixtures

## `historical-corpus.jsonl` — hand-labeled ground-truth corpus

Per the Assignment in the Phase 4 design doc, the classifier must be
grounded against real historical PRs before shadow-mode validation.

**Format:** one JSON object per line.

```json
{
  "pr_number": 17,
  "pr_title": "chore: cargo fmt drift cleanup",
  "pr_url": "https://github.com/.../pull/17",
  "base_sha": "<sha>",
  "head_sha": "<sha>",
  "changed_paths": ["src/foo.rs", "src/bar.rs"],
  "human_decision": "merged",
  "expected_eligibility": "eligible",
  "expected_class": "fmt-equiv",
  "notes": "pure cargo fmt output, no semantic changes"
}
```

Fields:

- `pr_number`, `pr_title`, `pr_url` — provenance.
- `base_sha`, `head_sha` — the actual commit SHAs. Lets us reproduce the
  classifier run against the same diff state.
- `changed_paths` — output of `gh pr diff <n> --name-only` at the time.
- `human_decision` — what actually happened: `merged` | `closed` | `open`.
- `expected_eligibility` — hand-labeled by a human: `eligible` | `ineligible`.
  This is THE ground truth. The classifier's job is to match it.
- `expected_class` — if eligible: `fmt-equiv` (Phase 4.0) or `dead-code`
  (Phase 4.1). If ineligible: `null`.
- `notes` — 1-line why this was labeled the way it was.

## Procedure (the Assignment)

1. Run `gh pr list --state all --limit 30 --json number,title,url,mergedAt`
   to list the last 30 PRs.
2. For each PR, capture the diff: `gh pr diff <n> > /tmp/pr-<n>.diff`.
3. Read the diff by hand. Label `expected_eligibility`:
   - `eligible` + `fmt-equiv` if the entire diff is reproducible by running
     `cargo fmt` — no logic changes, no imports reshuffled, nothing but
     whitespace and formatting.
   - `ineligible` for everything else (logic changes, new functions, bug
     fixes, anything the classifier should NOT auto-merge).
4. Append one JSON line to `historical-corpus.jsonl`.
5. Run the integration test once the classifier is implemented:
   `cargo test -p classify-pr --test historical_corpus_replay`.

The classifier's precision and recall on this corpus is the gate for
entering shadow-mode on live PRs. Zero false positives required.
