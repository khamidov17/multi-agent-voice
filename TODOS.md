
## Phase 0 — deferred items (added by /autoplan on 2026-04-21)

- [ ] Define Sentinel-watches-Nova event schema (Phase 2 pre-work)
- [ ] Design post-failure forensic bundle command (`/forensic` or similar) that pulls last 5 min of logs + last metric snapshot + last journal entries into one Telegram message
- [ ] Revisit cross-model reviewer's "Sentinel-as-a-service" thesis at Phase 3 gate
- [ ] Verify Claude subprocess timeout enforcement in Phase 0's first session (scope in if missing)
- [ ] Revisit CC heartbeat cadence — do we actually need 10s over existing 60s? Defer decision to after Phase 0 observation data arrives.
- [ ] Nova SYSTEM.md prompt audit — any references to Edit/Write tools that need rewrite post-protected_write migration
- [ ] Consider consolidating the three "guardian" nouns (binary, event_type, key file) naming — picked to avoid collision but could be clearer
