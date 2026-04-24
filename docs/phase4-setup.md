# Phase 4 — Operator Setup

One-time configuration to enable GitHub-native auto-merge for mechanical PRs
(formatter drift, Phase 4.0; dead-code removal arrives in 4.1).

**Realistic time:** 15 min if you already have admin on the repo + a
GitHub App ready. 45–90 min otherwise. Do steps 0 and 1 FIRST, before
touching anything else — if step 0 fails, the whole design doesn't apply
to your repo.

## Step 0 — Prerequisites

Your GitHub account must be **admin** on the repo, and **not Nova's
account**. Nova must operate under a separate, non-admin bot identity
(ENG-D3 from the design review). If Nova had admin, Nova could disable
branch protection.

```bash
# Replace owner/repo with this repo.
gh api "/repos/khamidov17/multi-agent-voice" --jq '.permissions.admin'
# Must print: true
```

If you do not see `true`, stop here. Either get admin access, or fold
Phase 4 into someone else's account.

## Step 1 — Enable "Allow auto-merge"

```bash
gh api \
  --method PATCH \
  "/repos/khamidov17/multi-agent-voice" \
  -F allow_auto_merge=true \
  -F allow_squash_merge=true \
  -F squash_merge_commit_title="PR_TITLE" \
  -F squash_merge_commit_message="PR_BODY"

# Verify:
gh api "/repos/khamidov17/multi-agent-voice" --jq '.allow_auto_merge'
# Must print: true
```

If the command fails with `allow_auto_merge: forbidden`, the repo's
visibility or the plan doesn't support auto-merge. Private repos on the
free tier do not have this feature. You cannot proceed.

## Step 2 — Create the GitHub App for control-plane writes

Default `GITHUB_TOKEN` can write labels and statuses but **cannot** write
repo variables. The canary needs `actions:write` to set
`AUTOMERGE_ENABLED=0` on anomaly (ENG-D4 finding).

Create a GitHub App named "Trio Automerge Control" with these
permissions:

- **Actions:** Read & write
- **Contents:** Read
- **Pull requests:** Write
- **Checks:** Write
- **Issues:** Write
- **Metadata:** Read (required)

Install it on this repo only. Download the private key `.pem`. Store it
as a repo secret:

```bash
gh secret set AUTOMERGE_CTL_KEY < ~/Downloads/trio-automerge-control.*.private-key.pem
```

Also store the App ID and Installation ID as repo variables (non-secret
identifiers — the private key is the credential, these are just
addresses):

```bash
gh variable set AUTOMERGE_CTL_APP_ID --body <your-app-id>
gh variable set AUTOMERGE_CTL_INSTALL_ID --body <your-install-id>
```

**Simpler alternative** if GitHub Apps feel heavy: create a fine-grained
PAT with `actions:write` + `contents:read` + `pull-requests:write` +
`checks:write` + `issues:write` on this repo only. Set 90-day expiry.
Calendar a rotation reminder.

```bash
gh secret set AUTOMERGE_CTL_TOKEN --body <your-pat>
```

The workflows read `secrets.AUTOMERGE_CTL_TOKEN` first, `secrets.GITHUB_TOKEN`
as fallback.

## Step 3 — Initialize `AUTOMERGE_ENABLED=0` (fail-safe start)

**Positive polarity: only the exact string `"1"` un-pauses.** Missing,
empty, `"0"`, `"true"`, anything else = paused (classifier fails safe).

```bash
gh variable set AUTOMERGE_ENABLED --body 0
# Verify:
gh variable list | grep AUTOMERGE_ENABLED
```

## Step 4 — Configure branch protection on `main`

Branch protection requires `automerge/classified-safe` status check +
your named CI workflows. This is the SOLE enforcement primitive — the
label is advisory (ENG-4). Renaming a required check silently lowers
the bar (ENG-F2); the weekly audit workflow catches drift.

**Required-checks allowlist** — keep this fenced block in sync with live
config. The `required-checks-audit.yml` workflow diffs them weekly.

```required-checks-allowlist
automerge/classified-safe
```

Wire it via `gh`:

```bash
gh api \
  --method PUT \
  "/repos/khamidov17/multi-agent-voice/branches/main/protection" \
  -f required_status_checks[strict]=true \
  -f required_status_checks[contexts][]="automerge/classified-safe" \
  -f enforce_admins=true \
  -f required_pull_request_reviews[required_approving_review_count]=0 \
  -f required_pull_request_reviews[dismiss_stale_reviews]=false \
  -f restrictions=null
```

**`enforce_admins=true` is critical.** Without it, the owner's admin PAT
can bypass the gate — which also means a compromised Nova with admin
tokens bypasses it (ENG-D3).

## Step 5 — Merge the Phase 4 PR

Merge the PR containing:

- `tools/classify-pr/` (the crate)
- `.github/workflows/automerge-*.yml` + `shadow-mode-reconcile.yml` + `required-checks-audit.yml`
- `docs/phase4-*.md` (setup / runbook / debugging)
- `rust-toolchain.toml`

## Step 6 — Graduation checklist (shadow-mode)

`AUTOMERGE_ENABLED=0` means the classifier runs but never sets the
status check to `success`. It logs would-have-been verdicts only.

Watch for 10 real PRs to close. After each PR closes, the
`shadow-mode-reconcile.yml` workflow appends a row to
`tools/classify-pr/state/shadow-decisions.jsonl`.

Shadow-mode exits when all of the following hold:

- [ ] ≥10 rows in `shadow-decisions.jsonl`.
- [ ] **Zero `false_positive` matches** (classifier said eligible + human did not merge).
- [ ] Classifier's `win` count is >0 (at least one eligible-and-merged PR).

When satisfied:

```bash
gh variable set AUTOMERGE_ENABLED --body 1
```

Next eligible Nova PR auto-merges.

## Step 7 — Watch the kill criterion

30 days after enabling, measure: did human merge interventions on
classifier-eligible PRs drop by ≥50%? If no, delete
`.github/workflows/automerge-*.yml` and `tools/classify-pr/` — the whole
phase is a misfit and the opportunity cost against Phase 5 wasn't worth it.

## Operator-account identity

Nova (the AI that opens PRs) authenticates as a dedicated bot account —
e.g., `trio-automerge-bot@users.noreply.github.com`. This account has:

- **Read-only** on the repo (Nova writes via MCP tools to its own
  worktree; the push to the PR branch goes through `git push` under Nova's
  identity, which requires push access to non-protected branches).
- **No admin rights** — cannot edit branch protection, cannot disable
  workflows, cannot edit repo secrets.

Owner's admin account is separate. Admin tasks (including `gh pr merge
--admin` emergency overrides per `docs/phase4-runbook.md`) go through the
owner's identity, not Nova's.
