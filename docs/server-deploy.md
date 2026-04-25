# Server Deploy Runbook

What to do on the deploy box (DigitalOcean droplet `159.89.132.178`)
after PR #1 lands. The Phase 4 setup itself lives in
`docs/phase4-setup.md`; this doc covers the binary-deployment side:
build, restart, switch off Atlas, verify.

## Reality on the deploy box (audited 2026-04-25)

The runbook initially assumed a tidy systemd-managed deploy, but the
actual layout on `159.89.132.178` is messier and worth documenting so
future deploys don't waste time:

- **Repo** lives at `/home/ava/trio/` (NOT `~/trio-local/`).
- **Runtime data** lives at `/home/ava/trio-local/` (logs, sqlite DBs,
  guardian sockets, worktrees) — that's the only thing in `~/trio-local/`.
- **Bots are launched via raw `nohup` bash one-liners**, NOT via systemd.
  PPID is 1, no tmux, no screen. The systemd units `ava-nova` /
  `ava-atlas` / `ava-security` exist but are STALE — they reference an
  old `/home/ava/ava-agents/target/release/claudir` path that no longer
  exists. Don't bother with `systemctl start ava-nova`; use the bash
  pattern below.
- **Guardian was started the same way** — no `bootstrap-guardian.service`
  exists; the running guardian is a manually `nohup`'d process.
- **GitHub auth is not configured** on the box — `git fetch` fails with
  `could not read Username for 'https://github.com'`. For one-off
  deploys, ship updates via `git bundle` from the local Mac (see Step 1
  below). For long-term, set up a deploy key + SSH-based remote, or
  configure `gh auth login`.

Assumes:
- You have SSH access to the deploy box (key in `~/.ssh/` on your Mac,
  authorized in `/home/ava/.ssh/authorized_keys` on the box).

## 1 — Get the latest code onto the box

**If GitHub auth is set up** (PAT or SSH key configured):

```bash
ssh ava@159.89.132.178
cd /home/ava/trio
git fetch origin
git checkout phase-0   # or main once we land it
git pull --ff-only
```

**If GitHub auth is NOT set up** (current state — see "Reality" section
above), ship updates via a `git bundle` from your local Mac:

```bash
# On your local Mac, with the latest commits already pushed to origin:
git bundle create /tmp/trio-update.bundle phase-0
scp /tmp/trio-update.bundle ava@159.89.132.178:/tmp/

# Then on the box:
ssh ava@159.89.132.178
cd /home/ava/trio
# Save any deploy-only dirty changes first.
mkdir -p ~/.trio-deploy-backup
git diff > ~/.trio-deploy-backup/deploy-dirty-$(date +%Y%m%d-%H%M%S).patch
git checkout -- .
git fetch /tmp/trio-update.bundle "+phase-0:refs/remotes/origin/phase-0"
git reset --hard origin/phase-0
rm /tmp/trio-update.bundle
```

## 2 — Build

```bash
ssh ava@159.89.132.178
cd /home/ava/trio

# rust-toolchain.toml pins 1.90. First build after the pin bump pulls the
# toolchain via rustup (~30s download). On a 2 GB / 1 vCPU droplet, the
# trio crate alone takes ~22 min cold; subsequent rebuilds are ~1-2 min.
# Plan for half an hour total on first deploy.
cargo build --release
cd bootstrap-guardian && cargo build --release && cd ..
cd tools/classify-pr && cargo build --release && cd ../..

# Verify the new binaries.
ls -lh target/release/trio \
       bootstrap-guardian/target/release/bootstrap-guardian \
       bootstrap-guardian/target/release/guardianctl \
       tools/classify-pr/target/release/classify-pr
```

If `cargo build` fails, do NOT proceed. Read the error, fix, retry.
Common cause: the server's `rustup` is out of date — run
`rustup update` then retry.

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

## 4 — Restart the guardian + Nova (raw `nohup` pattern)

The systemd units are stale. Use raw `nohup` to match what's actually
launching the bots today.

```bash
# 4a — kill any running guardian (PID changes per run, find with pgrep)
GUARDIAN_PID=$(pgrep -f bootstrap-guardian)
if [ -n "$GUARDIAN_PID" ]; then
  kill "$GUARDIAN_PID"
  for _ in 1 2 3 4 5; do kill -0 "$GUARDIAN_PID" 2>/dev/null || break; sleep 1; done
fi
rm -f /home/ava/trio-local/run/bootstrap-guardian.sock

# 4b — start the new guardian
# CRITICAL: --env dev. The new binary defaults to TRIO_ENV=prod, but
# /home/ava/trio-local/guardian.json only has a `dev` block. Without
# the flag the guardian crashes with: 'config has no block for
# TRIO_ENV="prod"'. Same for guardianctl — set TRIO_ENV=dev as an env
# var or pass the right config path.
cd /home/ava/trio
nohup /home/ava/trio/bootstrap-guardian/target/release/bootstrap-guardian \
    --config /home/ava/trio-local/guardian.json \
    --env dev \
    > /home/ava/trio-local/logs/guardian.log 2>&1 &
disown
sleep 2

# 4c — verify guardian is up (note TRIO_ENV=dev for guardianctl too)
TRIO_ENV=dev /home/ava/trio/bootstrap-guardian/target/release/guardianctl \
    --config /home/ava/trio-local/guardian.json \
    status
# Should print: socket: /home/ava/trio-local/run/bootstrap-guardian.sock ... OK
#               pause flag: absent

# 4d — start Nova (Atlas stays disabled per Step 3)
RUST_LOG=info nohup /home/ava/trio/target/release/trio /home/ava/trio/nova.json \
    > /home/ava/trio-local/logs/nova.log 2>&1 &
disown
sleep 4
pgrep -af "/trio /home/ava/trio/nova.json"
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
