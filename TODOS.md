
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
