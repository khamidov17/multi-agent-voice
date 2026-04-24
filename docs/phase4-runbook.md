# Phase 4 — Operator Runbook

What to do when auto-merge is paused, when you need to land something
against the classifier, or when you see a `phase4-alert` issue.

## Pause recovery (after canary triggered)

The canary opens an issue titled
`phase4-alert: canary paused auto-merge after …` and flips
`AUTOMERGE_ENABLED=0`. Work the checklist in the issue body:

- [ ] **Confirm attribution.** Read the failing workflow run. Did the
  failure actually come from the auto-merged commit, or from a concurrent
  human commit the canary misattributed? Run:
  ```bash
  git log --first-parent main --since="30 minutes ago" --format='%h %ae %s'
  ```
  If the failing commit's author is not one of the known auto-merge bot
  identities, this is a false-positive pause. Note in the issue, then set
  `AUTOMERGE_ENABLED=1` and move on.

- [ ] **Fix `main`.** Revert the offending commit, OR land a forward-fix.
  Do NOT leave `main` red.
  ```bash
  git revert <auto-merged-sha>
  git push origin main
  ```

- [ ] **Verify CI is green.** Wait for the next workflow run on `main`
  to pass.

- [ ] **Re-enable.**
  ```bash
  gh variable set AUTOMERGE_ENABLED --body 1
  ```

- [ ] **Verify next PR.** Open a trivial fmt-only PR (or wait for Nova to
  open one). Within 5 minutes the `automerge/classified-safe` status
  check should post `success`. If not, diagnose via
  `docs/phase4-debugging.md`.

- [ ] **Close the `phase4-alert` issue** with a one-line post-mortem:
  what broke, what fixed it, any follow-up action.

## Emergency merge (owner override)

The classifier is wrong or broken and you need to land a PR NOW.

### Tier 1 — normal override

Use the owner's admin account:

```bash
gh pr merge <number> --squash --admin
```

`--admin` bypasses required checks. Logged in the repo audit log.
`enforce_admins=true` means even the owner has to explicitly opt in with
`--admin` — the setting does NOT remove the override, it just makes the
override explicit and auditable.

### Tier 2 — classifier disable (no merge change)

```bash
gh variable set AUTOMERGE_ENABLED --body 0
```

Classifier workflow still runs and still labels. But because
`AUTOMERGE_ENABLED != 1`, the verdict is always `PAUSED` with exit code
5 → status check = `failure`. Every PR reverts to manual review. Use
while iterating on classifier bugs.

### Tier 3 — break-glass (rare)

Classifier workflow itself is catastrophically broken (won't run, infinite
loop, token exhausted). Temporarily remove `automerge/classified-safe`
from branch-protection required checks:

```bash
# Remove the required check.
gh api --method PATCH \
  "/repos/khamidov17/multi-agent-voice/branches/main/protection/required_status_checks" \
  -f 'contexts[]=other-required-check-if-any'
```

Commit this action's rationale to an issue. The `required-checks-audit.yml`
workflow will open a `phase4-alert: drift detected` on its next weekly
run if the allowlist in `docs/phase4-setup.md` is not also updated —
that's a feature, not a bug. You want that alert to remind you to
reconcile.

## Per-PR override (surgical)

Classifier rejected a PR you know is safe. Don't disable the whole
classifier — apply the override label from the owner's account:

```bash
# Must be the owner account; the workflow checks github.actor == owner.
# (If the per-PR override workflow hasn't been wired yet, this just
# adds a label — land it manually via `gh pr merge --admin` for now.
# Wiring the label into the workflow is a Phase 4.1 item.)
gh pr edit <number> --add-label "automerge:override"
```

Phase 4.0 does not honor this label automatically. It is forward-
compatible scaffolding — Phase 4.1 will teach `automerge-classify.yml`
to recognize the label when `github.actor` matches the owner identity,
and short-circuit the classifier.

For 4.0, override = `gh pr merge <n> --squash --admin`.

## Required-checks drift (weekly audit alert)

`required-checks-audit.yml` opened a `phase4-alert: branch-protection
required-checks drift detected`.

- [ ] Read the diff in the issue body.
- [ ] If the **doc is stale:** update the
  `required-checks-allowlist` fenced block in `docs/phase4-setup.md` to
  match the live config. Merge the update.
- [ ] If the **live config is wrong:** restore it via `gh api` to match
  the doc. Use the command in `docs/phase4-setup.md` Step 4.
- [ ] If a check was **renamed**: verify nothing unsafe slipped through
  during the rename window. Check PRs merged since the last audit with
  `gh pr list --state merged --search "merged:>YYYY-MM-DD"`.
- [ ] Close the issue with a one-line note.

## Pausing for a planned window (maintenance, investigation)

Same as Tier 2 emergency disable. Remember to re-enable:

```bash
gh variable set AUTOMERGE_ENABLED --body 0   # pause
# ... do the thing ...
gh variable set AUTOMERGE_ENABLED --body 1   # resume
```

There is no scheduled un-pause; the flag is sticky.

## Kill-switching Phase 4 entirely

30 days after shadow-mode ended, if the kill criterion (≥50% reduction in
human merge interventions on eligible PRs) did not hit:

```bash
# Remove required check from branch protection.
gh api --method PATCH \
  "/repos/khamidov17/multi-agent-voice/branches/main/protection/required_status_checks" \
  -f 'contexts[]=other-required-check-if-any'

# Disable and delete the workflows.
gh workflow disable automerge-classify.yml
gh workflow disable automerge-canary.yml
gh workflow disable shadow-mode-reconcile.yml
gh workflow disable required-checks-audit.yml
git rm .github/workflows/automerge-*.yml
git rm .github/workflows/shadow-mode-reconcile.yml
git rm .github/workflows/required-checks-audit.yml
git rm -rf tools/classify-pr/

# Remove the GitHub App + secrets.
gh secret delete AUTOMERGE_CTL_KEY
gh secret delete AUTOMERGE_CTL_TOKEN || true
gh variable delete AUTOMERGE_ENABLED
```

Write a 3-sentence post-mortem in TODOS.md explaining why the kill
criterion was missed (data volume, classifier bugs, workflow friction,
whatever). This data is load-bearing for whether to attempt Phase 4.1
(dead-code) or skip to Phase 5.
