# Server Deploy Runbook

What to do on the deploy box (`/home/ava/trio-local/` on the
DigitalOcean droplet) after PR #1 lands. The Phase 4 setup itself lives
in `docs/phase4-setup.md`; this doc covers the binary-deployment side:
build, restart, switch off Atlas, verify.

Assumes:
- You have SSH access to the deploy box.
- Repo lives at `/home/ava/trio-local/` (or wherever your `PROJECT_DIR`
  in `deploy/ctl.sh` points to).
- `deploy/ctl.sh` is the systemd-unit wrapper from your local box (was
  gitignored before — kept that way; per-bot service definitions are
  deploy-state, not repo-state).

## 1 — Pull main + build

```bash
ssh ava@159.89.132.178   # DigitalOcean droplet
cd ~/trio-local
git fetch origin
git checkout main
git pull --ff-only

# Build everything in release mode. First build after the rust-toolchain.toml
# bump (now pinned to 1.90) will pull the toolchain — about 30s. Subsequent
# rebuilds reuse it. Both crates build in parallel from the workspace.
cargo build --release -p trio
cargo build --release -p bootstrap-guardian
cargo build --release --manifest-path tools/classify-pr/Cargo.toml

# Verify the new binaries.
./target/release/trio --help 2>&1 | head -3 || true
./bootstrap-guardian/target/release/guardianctl status
./tools/classify-pr/target/release/classify-pr --version
```

If `cargo build` fails, do NOT proceed. Read the error, fix, retry.
Common cause: the server's `rustup` is out of date — run
`rustup update stable` then retry.

## 2 — Stop the running bots

```bash
sudo ./deploy/ctl.sh stop
# Or per-bot:
#   sudo systemctl stop ava-atlas
#   sudo systemctl stop ava-nova
#   sudo systemctl stop ava-security
```

`ctl.sh status` should show all three as `inactive` after this.

## 3 — Disable Atlas (per the 2026-04-25 decision)

Atlas is the public-chatbot-tier bot (Tier 2 in the architecture).
Today, the goal is to run only Nova during the Phase 4 shadow-mode
window — Atlas's feature surface is broad (~40 MCP tools, image
generation, TTS, focus mode) and concurrent failure modes there make
it harder to attribute Phase 4 incidents to the auto-merge pipeline
specifically. Re-enable Atlas after Phase 4's 30-day kill criterion
evaluates.

### Two options — pick one

**Option A — disable the systemd unit (recommended).** Survives reboot,
explicit, easy to reverse:

```bash
sudo systemctl stop ava-atlas
sudo systemctl disable ava-atlas
sudo systemctl is-enabled ava-atlas   # should print: disabled
```

Edit `deploy/ctl.sh` on the deploy box (it's gitignored locally) to
remove `atlas` from `BOTS` so `ctl.sh start/restart/status` no longer
touches it:

```bash
# /home/ava/trio-local/deploy/ctl.sh
- BOTS="atlas nova security"
+ BOTS="nova security"
```

Save. Now `sudo ./deploy/ctl.sh start` only starts Nova + security.

**Option B — rename atlas.json.** Atlas's binary is the same trio binary
fed a different JSON config. Renaming `atlas.json` to `atlas.json.disabled`
makes the systemd unit fail-fast on startup. Less explicit than option A
but doesn't require editing the systemd unit:

```bash
mv ~/trio-local/atlas.json ~/trio-local/atlas.json.disabled
sudo systemctl restart ava-atlas   # will fail; that's the point
sudo systemctl status ava-atlas    # confirm "Failed to start"
```

You'll still see the unit in `ctl.sh status` as failed. To suppress
that noise, prefer option A.

### Re-enabling Atlas later

```bash
# Option A reversal:
sudo systemctl enable ava-atlas
sudo systemctl start ava-atlas
# Edit deploy/ctl.sh: BOTS="atlas nova security"

# Option B reversal:
mv ~/trio-local/atlas.json.disabled ~/trio-local/atlas.json
sudo systemctl restart ava-atlas
```

## 4 — Restart the guardian + Nova

```bash
# Guardian first (Nova depends on the UDS socket).
sudo systemctl restart bootstrap-guardian
./bootstrap-guardian/target/release/guardianctl status
# Should print "running" with paused=false, allowed_uids=[<ava's uid>]

# Then Nova.
sudo systemctl start ava-nova
sudo ./deploy/ctl.sh status
```

Expected `ctl.sh status` after restart:
```
=== ava-agents status ===

  atlas: disabled (or inactive — that's fine)
  nova: RUNNING (PID NNNNN, ~XXX MB)
  security: RUNNING (PID NNNNN, ~XXX MB)

  Shared DB: N messages, last: <recent timestamp>

  Heartbeats:
  bot_name | last_heartbeat | status | current_task
  nova     | <ISO datetime> | active | ...
  security | <ISO datetime> | active | ...
```

If Nova doesn't reach `active` within 60s, tail the log:
```bash
sudo ./deploy/ctl.sh logs nova
# Or directly:
tail -f /home/ava/trio-local/data/nova/logs/trio.log
```

Common startup failures and what they mean:
- **"Security violation" with `mcp__claude_ai_*`** — should be fixed by
  the runtime-fix commit ([63fe7ad](#)). If it recurs, Claude Code may
  have added a NEW first-party MCP prefix not yet whitelisted; grep
  `setup_claude_process` for the `starts_with("mcp__claude_ai_")` line.
- **`RLIMIT_NOFILE`-related auth failures** — should be fixed by
  [e891e9c](#) (raise to 10240). If it recurs, the systemd unit's
  `LimitNOFILE` is overriding our runtime bump.
- **Guardian UDS not ready** — Nova retries for up to 5s; if guardian
  takes longer to boot, restart Nova once.

## 5 — Verify Phase 4 plumbing (optional but recommended)

Confirm the classifier can run against a sample diff:

```bash
cd ~/trio-local
echo "README.md" | ./tools/classify-pr/target/release/classify-pr check \
  --repo-root . \
  --stdin-paths \
  --automerge-enabled 1 \
  --verbose
# Expected: eligible=true, reason_code=Ok (because README.md isn't
# protected and `cargo fmt --check` passes on a clean main).

# And confirm the protected-path gate works:
echo "bootstrap-guardian/src/main.rs" | ./tools/classify-pr/target/release/classify-pr check \
  --repo-root . \
  --stdin-paths \
  --automerge-enabled 1 \
  --verbose
# Expected: eligible=false, reason_code=ProtectedPath, exit code 3.
```

If those two smoke tests don't behave as documented, do NOT proceed
with the GitHub-side Phase 4 setup (`docs/phase4-setup.md`) — the
classifier on disk is broken and you'll be debugging from the wrong
end of the pipeline.

## 6 — Hand-label the historical corpus

Per `tools/classify-pr/fixtures/README.md`, the classifier needs ground
truth before shadow-mode validation can mean anything. Pick the last 30
PRs from the repo's history and label each as
`expected_eligibility: eligible | ineligible`. Append one JSON line per
PR to `tools/classify-pr/fixtures/historical-corpus.jsonl`. Estimated
time: ~2 hours.

Commit the corpus as its own PR (PR #4 in `docs/nova-pr-queue.md` is the
test-backfill round, but the corpus PR can land before any of those).

## 7 — Wire up Phase 4 GitHub-side

Switch to `docs/phase4-setup.md` for the GitHub App / branch-protection /
`AUTOMERGE_ENABLED=0` initialization. None of those need access to the
deploy box — they're configured via `gh` CLI from any machine
authenticated against the repo.

## 8 — Sanity-check after first day of running

```bash
# 24h after deploy:
sudo ./deploy/ctl.sh status
# - Nova should still be active.
# - No phase4-alert issues opened on GitHub (canary didn't fire).
# - Heartbeats current.

# Tail Nova for any 'protected_write' denials or alerts:
grep -E 'protected_write|guardian\.deny|phase4-alert' \
  /home/ava/trio-local/data/nova/logs/trio.log | tail -30

# And the guardian's own audit log:
sudo cat /opt/trio/run/audit.jsonl | tail -30
# (or wherever your bootstrap-phase0.sh installed run-state — check
#  /home/ava/trio-local/data/guardian/audit.jsonl as a fallback.)
```

If anything looks wrong, `docs/phase4-runbook.md` has the pause-recovery
checklist. If you don't trust the pipeline, set `AUTOMERGE_ENABLED=0`
via `gh variable set` (you can do this from anywhere — it's a GitHub
state change, no shell on the deploy box required).

## Quick reference — the commands you'll re-run

```bash
# Deploy refresh after a new PR lands on main:
ssh ava@159.89.132.178   # DigitalOcean droplet
cd ~/trio-local && git pull --ff-only && \
  cargo build --release -p trio && \
  cargo build --release -p bootstrap-guardian && \
  cargo build --release --manifest-path tools/classify-pr/Cargo.toml && \
  sudo systemctl restart bootstrap-guardian ava-nova ava-security && \
  ./deploy/ctl.sh status

# Pause auto-merge globally (no SSH needed):
gh variable set AUTOMERGE_ENABLED --body 0

# Resume:
gh variable set AUTOMERGE_ENABLED --body 1
```
