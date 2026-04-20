# Bootstrap Guardian — architecture + operations

A reader-friendly companion to [`bootstrap-guardian/README.md`](../bootstrap-guardian/README.md). This document is the one to link from `CLAUDE.md`.

## The problem in one paragraph

Nova (Tier 1 CTO bot) has `Edit, Write, Bash` in its Claude Code tool string. When Nova edits its own harness source and ships a bad change overnight, the harness breaks and Nova cannot un-break itself, because Nova is the process that just broke. The bootstrap guardian is the answer: a separate long-running process that owns write permission on a short list of "bootstrap-critical" paths and exposes a narrow, authenticated write RPC. Nova asks; guardian decides.

## Block diagram

```
 Owner (ava)                                   guardian.key (0400)
     │                                              ▲
     │ /dev/tty                                     │ reads once at start
     ▼                                              │
  guardianctl ──── admin RPC ──▶ bootstrap-guardian │
                                      ▲ (owns  ────┘
                                      │  protected  )
                                      │  paths
                                      │  SO_PEERCRED)
                                      │
                  UDS newline-JSON    │
  ┌──── harness ─── protected_write ──┘           ┌──── /opt/nova/data/...
  │    (future                                     │     (allowed root)
  │     MCP tool)                                  │
  │                                                │
  │                                                │     /opt/nova/src/main.rs
  │                                                └──── (protected)
  └── Claude Code (Edit/Write removed)
      ↳ if asked to write a file, calls protected_write tool
      ↳ tool signs request, sends to guardian
      ↳ guardian decides allow/deny, writes on behalf
```

**Core invariant:** No process other than the guardian (and UID-separate `root`) can write to any path in `protected_paths`.

## Request lifecycle

1. Harness's MCP `protected_write` tool (landing in the next slice) serializes `{op, path, bytes, nonce}`.
2. It signs: `hmac = hmac-sha256(key, op || "|" || path || "|" || sha256(bytes) || "|" || nonce_le)`.
3. It opens UDS `<run_dir>/bootstrap-guardian.sock`, writes one JSON line, reads one JSON line back.
4. Guardian reads `SO_PEERCRED` / `getpeereid` — rejects if UID not in `allowed_uids`.
5. Guardian checks pause flag — if present, responds `Paused`.
6. Guardian parses the JSON — if malformed, responds `Malformed`.
7. Guardian recomputes HMAC, constant-time compares — if mismatch, responds `BadHmac`.
8. Guardian consults `nonces.db` — if replay, responds `ReplayDetected`; else records `nonce`.
9. Guardian canonicalizes `path` via `fs::canonicalize` (or rebuild-from-ancestor for missing files). Rejects `..` in the canonical form (defense in depth).
10. Guardian checks canonical path against `protected_paths` (blocklist) and `allowed_roots` (allowlist).
    - Hits a protected root → `Denied` with `alternative_roots` hint.
    - Outside every allowed root → `Denied`.
    - Else → proceed.
11. Guardian `std::fs::OpenOptions::new().create().write().truncate().custom_flags(O_NOFOLLOW).open()` — defeats symlink swap at the very last moment.
12. Guardian `write_all + sync_all`.
13. Guardian appends a JSON line to `guardian.audit.jsonl` with `{ts, uid, op, path (canonical), decision, bytes, reason}`.
14. Guardian returns `{ ok: true, written_bytes: N }`.

## Config

A single JSON file with two top-level blocks — `dev` and `prod`. The guardian reads `CLAUDIR_ENV` (default `prod`) to pick one.

Each block has:

| Key | Meaning |
|---|---|
| `run_dir` | Directory holding the UDS socket, `guardian.key`, `nonces.db`, and `paused` flag. Mode 0700, owned by guardian UID. |
| `protected_paths` | Canonical absolute paths the guardian refuses to write. Directories are recursive. |
| `allowed_roots` | Canonical absolute paths the guardian WILL write under. Every request's canonical path must start with one of these. |
| `allowed_uids` | Peer UIDs permitted to connect. Typically `[harness_uid]`. Non-members are rejected before any parse. |
| `request_timeout_secs` | Read/write timeout per request. Default 5. |

See `bootstrap-guardian/deploy/guardian.example.json` for a starting point.

## Operational procedures

### Start / stop

```bash
# macOS
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.claudir.bootstrap-guardian.plist
launchctl kickstart gui/$(id -u)/com.claudir.bootstrap-guardian
# stop
launchctl bootout gui/$(id -u)/com.claudir.bootstrap-guardian

# Linux
sudo systemctl enable --now bootstrap-guardian
sudo systemctl stop bootstrap-guardian
```

### Health check

```bash
./bootstrap-guardian/target/release/guardianctl \
  --config /opt/claudir/guardian.json status
```

The `status` subcommand performs a full signed Ping RPC, so a green status means the socket is reachable AND the key matches AND the guardian is accepting requests.

### Emergency pause (break-glass)

If the guardian misbehaves — wrong config, denying legitimate writes — you don't have to kill the process.

```bash
guardianctl pause 30m --reason "investigating why Nova can't write reflections"
```

This creates `<run_dir>/paused`. Every subsequent request gets `Paused`. The harness will log the rejection in its own journal. To resume:

```bash
guardianctl resume
```

**Note:** pause deny-all is deliberately restrictive. It is NOT a bypass — it still stops Nova from writing. Use it as "halt the guardian's control" while you investigate, not "let Nova through." For a one-shot bypass, `override-once` is the intended tool; it is stubbed and tracked in TODOS.md.

### Rotating the shared key

If `guardian.key` may be compromised:

1. `systemctl stop bootstrap-guardian` (or launchctl bootout)
2. `head -c 64 /dev/urandom > /opt/claudir/run/guardian.key && chmod 0400 /opt/claudir/run/guardian.key`
3. Restart guardian
4. Reload the harness so it re-reads the key (tracked as a TODO — harness integration pending)

### Reading the audit log

```bash
tail -n 100 /opt/claudir/run/guardian.audit.jsonl | jq .
```

One line per decision. Fields: `ts`, `uid`, `op`, `path` (canonical when resolved), `decision` (`allow` / `denied_protected` / `denied_outside` / `bad_hmac` / `replay` / `uid_mismatch` / `paused` / `malformed` / `io_error` / `traversal`), `bytes`, `reason`, `err`.

A noisy `denied_protected` cluster is Nova trying to touch things it shouldn't — promote to a journal event in the harness and investigate.

## Failure modes and operator response

| Mode | Symptom | Response |
|---|---|---|
| Guardian crashes | Socket connection refused | launchd/systemd restarts with 5s backoff. If it crashes repeatedly, check `stderr` — likely config error (paths don't exist, key mode wrong). |
| Key file permissions drift | Guardian refuses to start with `mode 0NNN; must be 0400` | `chmod 0400 /opt/claudir/run/guardian.key`. |
| Harness cannot reach socket | `protected_write` returns `IpcTimeout` | Check guardian is running + `run_dir` is 0700 + socket is 0600 + owned by guardian UID. |
| Nonce drift after guardian DB loss | `ReplayDetected` on every request | Only the harness should care — harness-side nonce counter must be stored persistently. Restart both. Tracked in TODOS.md. |
| Disk full | `IoError` with `ENOSPC` | Free disk. No automatic recovery. |
| iCloud path on macOS | Various unpredictable failures | Run `run_dir` off iCloud. The bootstrap script does this by default (`~/claudir-dev/run` for dev). |

## What the guardian does NOT protect against

- **OS root.** Anyone with root (`sudo`) can edit any file regardless of the guardian.
- **Physical access.** Guardian runs only while online; offline file edits bypass everything.
- **Nova acquiring guardian UID.** OS-level user separation matters — run guardian as `claudir-guardian`, harness as `claudir-harness`.
- **Nova shelling out.** If the MCP tool architecture lets Nova run `sh` (via the harness `Bash` tool), Nova can `rm -rf` guardian files. The next Phase 0 slice removes `Edit`, `Write`, AND `Bash` from Nova's CC tool string; the guardian only addresses `Edit`/`Write`. `Bash` removal is a separate (important) policy choice tracked in the design doc.

## Roadmap

1. **This slice (shipped).** Standalone guardian binary + guardianctl + tests + scripts.
2. **Next slice (not in this branch).** Remove `Edit`/`Write` from Nova's Claude Code tool string. Add harness-side MCP `protected_write` tool. Feature flag + 48h shadow mode where both paths coexist.
3. **Slice after.** Integrate guardian events into `journal.rs` as `guardian.deny` / `guardian.allow` / `guardian.error` entries so the failure forensic bundle the CEO review recommended can span Nova + guardian.
4. **Later.** `override-once` CLI. Key rotation with harness-side reload. Sentinel watches the audit log.
