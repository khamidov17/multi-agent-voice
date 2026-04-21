
## Phase 0 — deferred items (added by /autoplan on 2026-04-21)

- [ ] Define Sentinel-watches-Nova event schema (Phase 2 pre-work)
- [ ] Design post-failure forensic bundle command (`/forensic` or similar) that pulls last 5 min of logs + last metric snapshot + last journal entries into one Telegram message
- [ ] Revisit cross-model reviewer's "Sentinel-as-a-service" thesis at Phase 3 gate
- [ ] Verify Claude subprocess timeout enforcement in Phase 0's first session (scope in if missing)
- [ ] Revisit CC heartbeat cadence — do we actually need 10s over existing 60s? Defer decision to after Phase 0 observation data arrives.
- [x] ~~Nova SYSTEM.md prompt audit — any references to Edit/Write tools that need rewrite post-protected_write migration~~ (done 2026-04-21: no hardcoded SYSTEM.md exists for Nova — only README.md + conversation_summary.md. Tool-string flip carries no prompt-rewrite burden.)
- [ ] Consider consolidating the three "guardian" nouns (binary, event_type, key file) naming — picked to avoid collision but could be clearer

## Phase 0 — still open after slices 1-5

- [ ] Dedicated journal writer task (HC2 from Eng review). Phase 0 events emit from dispatch; engine's own journal writes still share `Mutex<Connection>` with `compress_old_entries` + `search_journal`. Refactor to an mpsc-fed writer task is its own focused change.
- [ ] `MessageSender` trait + separate `tg.send` events emitted from `telegram.rs` / `tool_dispatch/messaging.rs`. Current coverage via `tool_call` entries captures success/error; HTTP-status-level detail deferred.
- [ ] Main-crate integration tests at `tests/phase0_*.rs`. Guardian crate has 28 end-to-end tests; main crate has 4 unit tests for `guardian_client` but no live protected_write round-trip.
- [ ] `observability-wishlist.txt` — owner-only assignment. Not an AI task.
- [ ] Live server-side smoke test on the RTX 3090: install via `scripts/bootstrap-phase0.sh`, `guardianctl status`, flip `nova_use_protected_write = true` in nova.json, verify Nova uses `protected_write` on a real task.
- [ ] Regression eval: Nova multi-file task completion rate with `nova_use_protected_write=false` vs `true`. Acceptance: completion rate drop ≤ 10%.

## Phase 0 — deferred from /review on 2026-04-21 (follow-up PRs)

### Guardian hardening

- [ ] Swap `std::thread::spawn` in `bootstrap-guardian/src/server.rs::run` for a bounded thread pool. Current unbounded spawn is an accept-flood DoS vector (flagged by /review adversarial).
- [ ] `openat2(RESOLVE_NO_SYMLINKS|RESOLVE_BENEATH)` on Linux for full-path TOCTOU defense. Current `O_NOFOLLOW` only checks the final component; intermediate symlink swaps between `canonicalize` and `rename` can still escape. macOS has no equivalent, so this is Linux-only hardening. (/review security + adversarial)
- [ ] Periodic `stat` of `guardian.key` in the harness client to detect mode drift (0400 → 0644 etc.) between boots. Currently the key is read once and never re-verified. (/review security)
- [ ] Socket-file ownership check: refuse to unlink a stale socket unless `meta.uid() == geteuid()`. Currently `remove_file` trusts the existing inode unconditionally. (/review security)
- [ ] Collapse `NonceStore::consume` SELECT+UPDATE into one atomic UPSERT with `RETURNING`. Current two-statement form is safe only because of the Mutex; a future multi-connection refactor silently regresses. (/review security)
- [ ] HMAC-verify ordering: move UID / paused / malformed checks AFTER HMAC verify (or return indistinguishable `Malformed` for all pre-HMAC rejects) to prevent timing side-channels on allowed_uids enumeration. (/review security)
- [ ] `override-once` state file alongside `override.key` tracking last-used nonce, so two back-to-back invocations in the same nanosecond don't silently fail. (/review security)
- [ ] Pre-commit hook: broaden regex coverage (GitHub PATs `ghp_`, AWS `AKIA*`, Slack `xox[baprs]-`, JWTs, high-entropy hex). Better: wrap a maintained tool (`gitleaks` / `trufflehog`). Current patterns only cover Telegram/Anthropic/OpenAI keys. (/review security)
- [ ] **CI secret scan:** pre-commit is bypassable via `git commit --no-verify`. Add a GitHub Actions job that greps the diff for credential shapes and fails the PR. Complements the pre-commit (which catches 90% of accidents) with a push-protection (which catches determined or forgetful contributors). (/review security)
- [ ] Rebuild missing tail components by re-canonicalizing each segment in `resolve_even_if_missing`. Currently only the deepest existing ancestor is canonicalized; tail segments are appended raw and a race can create symlinks in the tail between canonicalize and open. (/review security)
- [ ] Nonce seed: mix random 32 bits into the u128→u64 cast so clock skew (NTP step, VM restore) doesn't create a predictable replay-detected lockout. Also persist last-used nonce to disk so a reboot can pick up where we left off. (/review security + adversarial)

### Performance

- [ ] **HC2: dedicated journal writer task.** Current `journal::emit` holds `Mutex<Database>` across a synchronous SQLite insert in the dispatch hot path, serializing dual-lane execution. Fix: mpsc channel to a dedicated writer task owning its own `Connection`. (Phase 0 events use the new `emit` helper; the engine's existing journal writes via `compress_old_entries` / `search_journal` are the other half.) (/review performance + adversarial — high-confidence multi-specialist finding)
- [ ] Proper log rotation size cap via the `file-rotate` crate OR a custom `MakeWriter`. Current daily-only rotation shipped; the "100 MiB cap" in the Phase 0 design doc is NOT enforced until this lands. (/review performance)
- [ ] Drop `GuardianClient::connect_lock` in favor of per-request fresh connections via `spawn_blocking`'s thread pool (or switch to a small connection pool with independent nonces). Serializes future high-volume `protected_write` bursts. (/review performance)
- [ ] `tracing_appender` consider `lossy=false` (backpressure) instead of current default `lossy=true`. We now log dropped-line counters, but the right production default depends on whether burst-log-loss is more or less bad than burst-latency. (/review performance)

### API / wire protocol

- [ ] Add `proto_version: Option<u32>` field to `Req`/`Resp` in `bootstrap-guardian/src/proto.rs` + mirror in `src/guardian_client.rs`. Current protocol has NO versioning — future evolution surfaces as `BadHmac`/`Malformed` with no diagnostic hint. (/review api-contract)
- [ ] Promote `ErrCode` to a typed enum on the client side in `src/guardian_client.rs`. Currently `map_resp` string-matches only `"denied"`; every other guardian ErrCode (Paused, ReplayDetected, OverrideDisabled, UidMismatch, IoError, IpcTimeout, Malformed, PathTraversal) collapses into a generic `WriteResult::Err`. (/review api-contract + maintainability)
- [ ] Extract a shared `guardian-proto` crate so `Req`/`Resp`/`Op`/`ErrCode` are defined once. Currently duplicated between guardian and harness with the pinned-fixture tests as the only drift detection. (/review api-contract + maintainability)
- [ ] Add `alias = "..."` annotations to `ErrCode` variants + doc "WIRE-STABLE" contract so a rename doesn't silently misroute responses. (/review api-contract)
- [ ] Document `OverrideWrite` wire format (op tag, separator, signature formula, which key) in `docs/bootstrap-guardian.md` so external override CLIs can be built without reading Rust source. (/review api-contract)
- [ ] Document idempotency contract: `protected_write` is at-least-once on transport error; retries with a new nonce may re-apply the same content. Consider adding a guardian-side request-id + reply-cache for at-most-once delivery. (/review api-contract)

### Testing

- [ ] Main-crate end-to-end integration tests at `tests/phase0_*.rs` — spawn a real guardian on a tempdir socket, build a real `GuardianClient`, call `execute_protected_write` through a real `ChatbotConfig`. Currently `bootstrap-guardian` has 13 integration tests but the main-crate dispatch layer has none. (/review testing)
- [ ] Unit tests for `execute_protected_write` gates (Tier-2 rejection, guardian-absent, empty path, relative path, empty reason, oversized content). Currently zero coverage on the new dispatch module. (/review testing)
- [ ] Unit tests for `sweep_old_logs` retention pass. (/review testing)
- [ ] Unit tests for `journal::emit` swallow-error path (poisoned mutex must not panic). (/review testing)
- [ ] Unit tests for the `spawn_blocking` panic branch + RPC timeout branch in `protected_write.rs`. Requires trait-based injection. (/review testing)
- [ ] `load_key` unit tests: mode != 0400/0600 is rejected; key < 32 bytes is rejected. (/review testing)

### Maintainability

- [ ] Split `execute_protected_write` (~180 lines) into `validate_request` / `dispatch_rpc` / `journal_outcome`. Current function interleaves hot path + failure paths. (/review maintainability)
- [ ] Group `ChatbotConfig`'s 22+ fields into sub-structs (`PermissionsConfig`, `CognitiveConfig`, etc.). God-object risk grows with each phase. (/review maintainability)
- [ ] Introduce `struct ClaudeSpawnConfig` so `setup_claude_process` stops taking 9 parameters behind `#[allow(clippy::too_many_arguments)]`. (/review maintainability)
- [ ] Drop "Phase 0" prefix from the user-visible description of the `protected_write` tool in `tools.rs`. Stale once Phase 0 becomes the default. (/review maintainability)
- [ ] Consolidate the three "guardian" nouns (binary `bootstrap-guardian`, journal event_type `guardian.allow/deny/error`, config key file `guardian.key`). (/review maintainability)
