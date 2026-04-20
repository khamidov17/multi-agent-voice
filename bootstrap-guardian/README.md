# bootstrap-guardian

Write-guarding process for Claudir. Prevents Nova from modifying its own harness, wrapper, or launch config, even when it has Claude Code `Edit`/`Write` tools.

## Why this exists

Claudir's Phase 0 plan introduced a bootstrap invariant: Nova must not be able to brick its own harness at 3am. The CEO/Eng/DX review found that a software-only guardian is bypassable as long as Nova's Claude Code subprocess has direct `Edit`/`Write` tools, because those call `fs::write` in the kernel and never cross any IPC boundary the guardian can see.

This crate is the first of two halves of the fix:

1. **This crate (`bootstrap-guardian`)**: a long-lived Rust binary that owns write permission to protected paths, accepts UDS requests from the harness, authenticates via HMAC, and writes files on the harness's behalf after a canonicalize+whitelist check.
2. **Next slice, not yet in this branch**: remove `Edit`/`Write` from Nova's Claude Code tool string and expose a harness-side MCP tool `protected_write(path, content, reason)` that signs a request and forwards it to this guardian. Until the MCP tool lands, Nova's tool surface is unchanged and the guardian is a standalone process validating the architecture.

See `docs/bootstrap-guardian.md` in the repo root for the full architecture write-up and the Phase 0 design doc for the rationale chain.

## Components

```
src/
  lib.rs         # re-exports; public surface
  proto.rs       # Req / Resp / ErrCode wire types (newline-JSON)
  config.rs      # GuardianConfig (per-env: dev / prod)
  auth.rs        # HMAC-SHA256 compute/verify + SO_PEERCRED / LOCAL_PEERCRED
  nonce.rs       # monotonic nonce persistence (SQLite, survives restart)
  paths.rs       # canonicalize + allowed/protected decider
  audit.rs       # append-only JSONL log of every decision
  server.rs      # UDS accept loop + per-connection handler
  main.rs        # binary entrypoint
  bin/guardianctl.rs  # owner-only break-glass CLI
tests/
  integration.rs # end-to-end: Allow, DenyProtected, DenyOutsideAllowed,
                 # PathTraversal, BadHmac, ReplayDetected, Paused, Ping,
                 # Malformed, key permission checks
examples/
  client.rs      # reference client shape for the future MCP shim
deploy/
  bootstrap-guardian.service.tmpl       # systemd (Linux)
  com.claudir.bootstrap-guardian.plist.tmpl  # launchd (macOS)
  guardian.example.json                 # per-env config template
```

## Quick start

```bash
# From the Claudir repo root:
./scripts/bootstrap-phase0.sh
# Edit the generated <run_dir>/../guardian.json to match your env.
# Then load the launchd plist (macOS) or enable the systemd unit (Linux) as prompted.
# Verify:
./bootstrap-guardian/target/release/guardianctl \
  --config /opt/claudir/guardian.json status
```

## Wire protocol

Newline-JSON, one request per line, one response per line.

Request:

```json
{
  "op": "write",
  "path": "/opt/nova/data/foo.txt",
  "bytes_b64": "aGVsbG8=",
  "nonce": 1713614923123456789,
  "hmac": "hex(hmac-sha256(key, op || path || sha256(bytes) || nonce_le))",
  "reason": "Nova is writing a memory file"
}
```

Response (success):

```json
{ "ok": true, "written_bytes": 5 }
```

Response (deny):

```json
{
  "ok": false,
  "err_code": "denied",
  "message": "path /opt/nova/src/main.rs is in a protected root",
  "suggested_action": "Pick a path inside one of `alternative_roots`. ...",
  "alternative_roots": ["/opt/nova/data", "/opt/nova/logs", "/tmp/claudir-scratch"]
}
```

See `src/proto.rs::ErrCode` for the full rejection taxonomy.

## Security model

- **HMAC-SHA256** over `op || "|" || path || "|" || sha256(bytes) || "|" || nonce_le` with a 32+ byte shared secret at `<run_dir>/guardian.key` mode 0400.
- **Monotonic nonce** per UID, persisted in `nonces.db`. Survives guardian restart. Replay rejected.
- **Peer UID check** via `SO_PEERCRED` (Linux) / `getpeereid` (macOS). UIDs not in `allowed_uids` are rejected before any parse.
- **Path canonicalization** via `fs::canonicalize` (or rebuild-from-nearest-existing-ancestor for new-file writes). Symlink escape and `..` traversal both defeated. Compare against canonical forms of protected paths and allowed roots.
- **O_NOFOLLOW** on the final open() to defeat last-moment symlink swap.
- **Audit log** (`guardian.audit.jsonl`) — one JSON line per decision, append-only.
- **Paused mode** — `guardianctl pause` writes a flag the server checks per request; `resume` removes it. Used for emergency bypass without taking the guardian down entirely.

## Known limitations (tracked in TODOS.md)

- `guardianctl override-once` is stubbed, not implemented. The MVP pause/resume is enough for break-glass; a one-shot write-bypass CLI is planned for the follow-up slice.
- Guardian is not itself guarded — if Nova somehow acquires guardian UID, it can alter the key. Deploy with OS-level separation (different user account).
- Log rotation for `guardian.audit.jsonl` is external (logrotate / newsyslog).
- MCP `protected_write` tool wiring in the harness has not yet been shipped. Until that lands, the guardian is exercised only by `cargo test` + manual `examples/client`.

## Testing

```bash
cd bootstrap-guardian
cargo test --all-features
# Integration tests bind real UDS sockets in per-test tempdirs — serial_test
# prevents parallel interference.
```

## License

MIT, same as the parent project.
