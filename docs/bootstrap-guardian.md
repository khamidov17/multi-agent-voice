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

A single JSON file with two top-level blocks — `dev` and `prod`. The guardian reads `TRIO_ENV` (default `prod`) to pick one.

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
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.trio.bootstrap-guardian.plist
launchctl kickstart gui/$(id -u)/com.trio.bootstrap-guardian
# stop
launchctl bootout gui/$(id -u)/com.trio.bootstrap-guardian

# Linux
sudo systemctl enable --now bootstrap-guardian
sudo systemctl stop bootstrap-guardian
```

### Health check

```bash
./bootstrap-guardian/target/release/guardianctl \
  --config /opt/trio/guardian.json status
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
2. `head -c 64 /dev/urandom > /opt/trio/run/guardian.key && chmod 0400 /opt/trio/run/guardian.key`
3. Restart guardian
4. Reload the harness so it re-reads the key (tracked as a TODO — harness integration pending)

### Reading the audit log

```bash
tail -n 100 /opt/trio/run/guardian.audit.jsonl | jq .
```

One line per decision. Fields: `ts`, `uid`, `op`, `path` (canonical when resolved), `decision` (`allow` / `denied_protected` / `denied_outside` / `bad_hmac` / `replay` / `uid_mismatch` / `paused` / `malformed` / `io_error` / `traversal`), `bytes`, `reason`, `err`.

A noisy `denied_protected` cluster is Nova trying to touch things it shouldn't — promote to a journal event in the harness and investigate.

## Failure modes and operator response

| Mode | Symptom | Response |
|---|---|---|
| Guardian crashes | Socket connection refused | launchd/systemd restarts with 5s backoff. If it crashes repeatedly, check `stderr` — likely config error (paths don't exist, key mode wrong). |
| Key file permissions drift | Guardian refuses to start with `mode 0NNN; must be 0400` | `chmod 0400 /opt/trio/run/guardian.key`. |
| Harness cannot reach socket | `protected_write` returns `IpcTimeout` | Check guardian is running + `run_dir` is 0700 + socket is 0600 + owned by guardian UID. |
| Nonce drift after guardian DB loss | `ReplayDetected` on every request | Only the harness should care — harness-side nonce counter must be stored persistently. Restart both. Tracked in TODOS.md. |
| Disk full | `IoError` with `ENOSPC` | Free disk. No automatic recovery. |
| iCloud path on macOS | Various unpredictable failures | Run `run_dir` off iCloud. The bootstrap script does this by default (`~/trio-dev/run` for dev). |

## What the guardian does NOT protect against

- **OS root.** Anyone with root (`sudo`) can edit any file regardless of the guardian.
- **Physical access.** Guardian runs only while online; offline file edits bypass everything.
- **Nova acquiring guardian UID.** OS-level user separation matters — run guardian as `trio-guardian`, harness as `trio-harness`.
- **Nova shelling out.** If the MCP tool architecture lets Nova run `sh` (via the harness `Bash` tool), Nova can `rm -rf` guardian files. The next Phase 0 slice removes `Edit`, `Write`, AND `Bash` from Nova's CC tool string; the guardian only addresses `Edit`/`Write`. `Bash` removal is a separate (important) policy choice tracked in the design doc.

## Wire format reference (for alternative clients)

**Current version:** `proto_version = 1` (see `bootstrap-guardian/src/proto.rs::PROTO_VERSION`).

Every request MUST set `proto_version`. The guardian accepts equal-or-older versions permissively and rejects strictly-newer versions with `ErrCode::Malformed`. Bump only for breaking wire-format changes.

### Signing formula

HMAC-SHA256 over the byte concatenation:

```
op_tag   || b"|" || path_utf8 || b"|" || sha256(bytes_raw) || b"|" || nonce_u64_le
```

Where `op_tag` is the serde-rename bytes:

| `Op` | op_tag bytes | Key |
|---|---|---|
| `Op::Write` | `b"write"` | `guardian.key` |
| `Op::Ping` | `b"ping"` | `guardian.key` |
| `Op::OverrideWrite` | `b"override_write"` | **`override.key`** (separate file) |

Output: lowercase hex of the 32-byte HMAC digest (64 hex chars).

The pinned fixture `key = [0u8..64u8], op = Write, path = "/a", bytes = b"x", nonce = 1` produces `c28f43f14294ab137e3be1662eb17ad95057fc90af682ef6df2fdbf880613892`. Both the guardian and harness-client crates have a matching test asserting this hex — it's the drift alarm.

### OverrideWrite

Requires a separate `override_key_path` configured in `guardian.json`. The file must be mode 0400 with ≥32 bytes, same rules as `guardian.key`. The key is **not** trusted to the harness — only to the human operator. `guardianctl override-once --path X --reason Y [--content Z]` is the reference client.

OverrideWrite still enforces:
- `allowed_uids` (the peer UID must be registered)
- Nonce replay protection (same nonce store as regular Write)
- Path canonicalization + `O_NOFOLLOW` + atomic tempfile-rename
- `allowed_roots` (cannot escape to paths outside the allowlist)

Only `DenyProtected` is bypassed.

### Idempotency contract

**`protected_write` is at-least-once.** If the transport fails mid-RPC (guardian wrote but the response didn't return), the client will retry with a new nonce and the write MAY re-apply the same content. This is safe for content-only writes (second write = same bytes) and for writes the caller can make idempotent in payload (include a timestamp or version in the content). It is NOT safe for sequence-dependent writes.

If you need at-most-once semantics, add a request-id + reply cache on the guardian side (not shipped; tracked in TODOS.md).

### Typed error codes (`err_code`)

Stable serde names. The harness's `ClientErrCode` mirrors these exactly. Variants added, never renamed — renames break downstream branching.

| err_code | Shape | Callers should |
|---|---|---|
| `denied` | `DenyProtected` (protected path) or outside `allowed_roots` | Read `alternative_roots`, retry with a compliant path, or request `guardianctl override-once` |
| `path_traversal` | Canonicalization failed; symlink loop or missing non-creatable ancestor | Check the path exists or has a creatable parent inside an allowed root |
| `bad_hmac` | Wrong key, tampered payload, or stale key on disk | Restart harness to reload key |
| `replay_detected` | Nonce ≤ guardian's highest-seen for this UID | Increment client counter and retry |
| `uid_mismatch` | Peer UID not in `allowed_uids` | Install-time misconfiguration — check unit file |
| `io_error` | Filesystem returned an error after auth passed | Check disk space + permissions on target |
| `ipc_timeout` | Socket saturated or guardian stalled | Back off, retry once; escalate to human if persistent |
| `malformed` | Bad JSON / bad base64 / proto_version too new | Check request shape; if version drift, upgrade guardian |
| `paused` | `guardianctl pause` is active | Wait for `guardianctl resume` |
| `override_disabled` | `OverrideWrite` but no override key configured | Install an override key or use a regular Write to an unprotected path |

## Roadmap

1. **This slice (shipped).** Standalone guardian binary + guardianctl + tests + scripts.
2. **Next slice (not in this branch).** Remove `Edit`/`Write` from Nova's Claude Code tool string. Add harness-side MCP `protected_write` tool. Feature flag + 48h shadow mode where both paths coexist.
3. **Slice after.** Integrate guardian events into `journal.rs` as `guardian.deny` / `guardian.allow` / `guardian.error` entries so the failure forensic bundle the CEO review recommended can span Nova + guardian.
4. **Later.** `override-once` CLI. Key rotation with harness-side reload. Sentinel watches the audit log.
