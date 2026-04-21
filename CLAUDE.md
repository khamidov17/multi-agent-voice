# Trio — Three-Tier Telegram Bot Architecture

## Phase 0 — Bootstrap Guardian (COMPLETE — 2026-04-21)

A new Rust crate lives at [`bootstrap-guardian/`](bootstrap-guardian/). It is the write-guarding process that prevents Nova from modifying its own harness, wrapper, or launch config when Nova gets woken up to autonomously ship code.

**Status right now (this branch):** all 5 slices shipped + 3 review-fix batches (P0-fix-A, P0-fix-B, P0-fix-C). `/review` on 2026-04-21 caught 18 critical findings across 6 specialist reviewers + adversarial. All 18 critical items either landed or are explicitly tracked in `TODOS.md` with rationale. Phase 1 (alert bus + triage report) starts next session as a separate PR. Notable fixes landed in the review pass:

- **`nova_use_protected_write` flag is now actually plumbed to `ClaudeCode`** — previously dead code, flipping it had no effect. Now the harness routes through `start_with_guardian` with the real flag.
- **Bash is dropped from Nova's tool string** when the flag is on, not just Edit/Write. Bash could read `guardian.key` (0400 but owned by the harness UID = Bash UID) and mint its own HMAC, bypassing the guardian entirely. Nova in locked mode now gets `Read,WebSearch` only + the MCP `protected_write` tool.
- **Bootstrap script rejects same-UID installs in prod** (`TRIO_ALLOW_SAME_UID=1` override for dev sandboxes). Without UID separation filesystem perms can't actually enforce the invariant.
- **Atomic writes** in the guardian via tempfile + rename. Previous `create+truncate+write` was not crash-safe and concurrent writes to the same path interleaved bytes. Now each call writes to `.tmp.<pid>.<nanos>` with `O_EXCL|O_NOFOLLOW`, fsyncs, and atomically renames over the target.
- **Two-phase nonce:** `would_accept` peek BEFORE op, `consume` AFTER successful op. A transient fs error no longer burns a nonce.
- **16 MiB request cap** on the guardian UDS read path to block unbounded-stream DoS.
- **UTF-8 safe truncation** in `journal.rs` — multi-byte detail strings no longer panic at byte 500.
- **Pinned HMAC wire-format fixture tests** on BOTH sides with matching hex. Previous test only checked length; drift was invisible.
- **Length caps + control-char sanitization** on model-supplied path/reason before journal emit.
- **Dead wrapper cleanup** — `ClaudeCode::start` + `spawn_process` removed; tool-string logic extracted into one `compute_allowed_tools` fn (the "MUST mirror" duplication comment is gone).
- **Dropped-log-line watcher:** tracing-appender's `error_counter` is polled every 60s and surfaced via `warn!` when it advances. Post-mortems can now tell "nothing happened" from "we lost N lines."
- **Real size-cap rotation** via the `file-rotate` crate — 100 MiB per file, up to 168 rotated files (7 days at 1 rotation/hour worst case). The earlier rename-based sweep was a no-op (Unix rename keeps fd on same inode); `file-rotate` owns the writer and reopens on rotation.
- **HC2: dedicated `JournalWriter` task** — `src/chatbot/journal.rs` now exposes a mpsc-fed writer with a 4096-slot bounded queue and its own SQLite connection. Hot-path emits in `tool_dispatch/` use the writer when present, fall back to synchronous `journal::emit` otherwise. The old pattern held `Mutex<Database>` across a synchronous INSERT in the dispatch path, serializing Nova/Atlas/Sentinel parallel tool-call journaling. The writer eliminates that contention.
- **Main-crate end-to-end integration tests** at `tests/phase0_protected_write.rs` — 4 `#[serial]` tests spawn a real guardian in a background thread and drive `GuardianClient::protected_write` through the same `spawn_blocking` path the dispatch uses. Covers allowed-write-lands-on-disk, protected-path-denied-with-alternatives, outside-allowed-root-denied, back-to-back-writes-both-succeed.

Shipped:
- Guardian binary + tests + break-glass CLI with pause/resume/status/**override-once**
- Harness-side `src/guardian_client.rs` library (HMAC-SHA256 signed UDS client)
- New config fields: `guardian_enabled`, `guardian_socket_path`, `guardian_key_path`, `nova_use_protected_write` (shadow-mode flag, default false)
- **MCP `protected_write(path, content, reason)` tool** in `src/chatbot/tools.rs` + dispatch handler at `src/chatbot/tool_dispatch/protected_write.rs`. Gated on Tier-1 + guardian-available; returns structured JSON with err_code + human_message + suggested_action + alternative_roots so Nova can reason about denials.
- **Nova's Claude Code tool string is now conditional.** When `nova_use_protected_write = true` AND `full_permissions = true`, CC spawns with `Bash,Read,WebSearch` (no Edit/Write). The runtime security check that rejects unexpected Claude-provided tools mirrors the new string. Flag default is `false` — Nova's behavior is unchanged until an operator flips it.
- **Journal Phase 0 entry types**: `tool_call`, `tg.send`, `guardian.allow`, `guardian.deny`, `guardian.error`. Best-effort `emit()` helper that logs failures via `tracing::warn!` instead of bubbling up.
- **Tool-call journal events** emitted from `tool_dispatch/mod.rs` after every tool invocation (variant name + success/error + redacted preview + bot_name tag).
- **Guardian journal events** (`guardian.allow` / `guardian.deny` / `guardian.error`) emitted from the `protected_write` dispatch.
- **`JournalWriter` task** (HC2 fix) — mpsc-fed writer task with its own `Connection`, drains events off the dispatch hot path. `ChatbotConfig.journal_writer: Option<Arc<JournalWriter>>` threads through. Set to `None` for sync-fallback (used by integration tests for deterministic assertions).
- **Log rotation** — `file-rotate` crate with `ContentLimit::BytesSurpassed(100 MiB)` + `AppendTimestamp::default(FileLimit::MaxFiles(168))`. Real size-cap enforcement; the old rename-based sweep is gone. Dropped-line counter polled every 60s.
- Deploy templates for launchd (macOS) + systemd (Linux), bootstrap + uninstall scripts, pre-commit hook blocking credential-shaped files.
- Full `docs/bootstrap-guardian.md` + `bootstrap-guardian/README.md` architecture write-ups.

What is still deferred (tracked in TODOS.md):

- **Telegram `MessageSender` trait + separate `tg.send` events.** Current coverage via `tool_call` entries captures which sends happened and whether they succeeded; splitting out HTTP-status-level events from the specific `bot.send_message` callsites was deferred.
- **`observability-wishlist.txt`** — the human owner's assignment (tail bots, break things, produce the wishlist). Not an AI task.
- Live server-side smoke test: `./scripts/bootstrap-phase0.sh` on the deploy box, `guardianctl status`, flip `nova_use_protected_write = true` in `nova.json`, watch Nova use `protected_write` instead of Edit/Write.
- Regression eval: Nova multi-file task completion rate with `nova_use_protected_write=false` vs `true`. Acceptance: completion rate drop ≤ 10%.
- See TODOS.md "Phase 0 — deferred from /review" for the full long-tail of hardening items (openat2 TOCTOU, HMAC ordering, proto_version bumps, shared `guardian-proto` crate, ChatbotConfig sub-structs, etc.).

**What you can do with the guardian today:** run `cargo test -p bootstrap-guardian` to verify the full decision matrix (Allow / DenyProtected / DenyOutsideAllowed / PathTraversal / BadHmac / ReplayDetected / UidMismatch / Paused / Malformed / Ping). Run `./scripts/bootstrap-phase0.sh` to install the launchd plist or systemd unit. Run `guardianctl status` after the guardian is up.

**Full architecture + operations:** [`docs/bootstrap-guardian.md`](docs/bootstrap-guardian.md). Full design-doc chain: `~/.gstack/projects/ava/ava-trio-main-design-20260421-003433.md`.

**Roadmap from here:**

| Slice | What ships | Nova behavior |
|---|---|---|
| Slice 1 (shipped this branch) | guardian binary, guardianctl, tests, scripts, docs | unchanged — Nova still has Edit/Write |
| Slice 2 (next) | Remove Edit/Write from Nova's CC spawn args. Add MCP `protected_write` tool in the harness. Feature flag + 48h shadow mode. | Nova writes via MCP tool → guardian enforces |
| Slice 3 | Journal extensions + log rotation + Telegram delivery events | Observability closes the delta |
| Graduation gate | 7 clean overnight runs with log-sufficient post-mortems | Phase 1 (alerting) unlocks |

## Overview

Three bots, three trust levels, one Rust binary (`trio`). Owner controls via Telegram.
Each bot = three OS processes: wrapper (crash recovery) → harness (Rust, Telegram I/O, MCP) → Claude Code (AI subprocess).

**Owner:** `8202621898`
**Group (bot_xona):** `-1003399442526`

## The Three Tiers

```
┌─────────────────────────────────────────────────────────────────────┐
│  TIER 0: Owner & Supervisor                                         │
│                                                                      │
│  Owner communicates via Telegram (bot_xona or DMs to Nova).          │
│  Supervisor = raw `claude` CLI on the server, used only for          │
│  manual intervention (emergency fixes, debugging). NOT the trio   │
│  binary. Most of the time it does nothing — exists as a fallback.    │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  TIER 1: Nova (Private Assistant / CTO)                              │
│  Token: NOVA_BOT_TOKEN                                               │
│  full_permissions: true  │  owner_dms_only: true                     │
│  Claude Code: Bash, Edit, Write, Read, WebSearch                     │
│  Model: claude-opus-4-6                                              │
│  Config: nova.json  │  Data: data/nova/                              │
│                                                                      │
│  Responsibilities:                                                   │
│  - CTO — manages code, architectural decisions                       │
│  - Monitor Atlas & Sentinel health (cross-bot heartbeat)             │
│  - Fix bugs, deploy updates, restart bots                            │
│  - Act as owner's proxy in bot_xona                                  │
│  - Full system access — only talks to owner and other bots           │
│                                                                      │
│  full_permissions = true → claude --dangerously-skip-permissions      │
│  Safe because owner_dms_only = true (no public users)                │
└─────────────────────────────────────────────────────────────────────┘
                              │
                     monitors & coordinates
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│  TIER 2: Atlas (Public Chatbot)                                      │
│  Token: ATLAS_BOT_TOKEN                                              │
│  full_permissions: false  │  owner_dms_only: false                   │
│  Claude Code: WebSearch, WebFetch only (NO code execution)           │
│  Config: atlas.json  │  Data: data/atlas/                            │
│                                                                      │
│  Responsibilities:                                                   │
│  - Handle Telegram messages (group + DMs)                            │
│  - ~40 MCP tools: messaging, memory, images, TTS, billing           │
│  - Spam filtering (regex prefilter + Haiku classifier)               │
│  - Focus mode (single-chat attention with cursor tracking)           │
│  - Report bugs via report_bug tool                                   │
│  - CANNOT execute code — social engineering → no RCE                 │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  TIER 2: Sentinel (Security Monitor)                                 │
│  Token: SENTINEL_BOT_TOKEN                                           │
│  full_permissions: false  │  owner_dms_only: true                    │
│  Claude Code: WebSearch only                                         │
│  Config: sentinel.json  │  Data: data/sentinel/                      │
│                                                                      │
│  Responsibilities:                                                   │
│  - Security auditing and monitoring                                  │
│  - Dependency vulnerability scanning                                 │
│  - Log analysis for suspicious activity                              │
│  - Only communicates with owner                                      │
└─────────────────────────────────────────────────────────────────────┘
```

## Three-Process Model

Every bot instance runs as three OS processes:

```
┌─────────────┐     ┌──────────────────────┐     ┌─────────────────┐
│   Wrapper    │────▶│   Harness (trio)  │────▶│   Claude Code   │
│  (crash      │     │   - Telegram I/O     │     │   (AI brain)    │
│   recovery)  │     │   - MCP tool server  │     │   - stdin/stdout│
│              │     │   - Spam filtering   │     │     streaming   │
│  Restarts    │     │   - Message queue    │     │   - Session     │
│  harness on  │     │   - Health monitor   │     │     persistence │
│  crash with  │     │   - Bot-to-bot bus   │     │   - Structured  │
│  exp backoff │     │                      │     │     output      │
└─────────────┘     └──────────────────────┘     └─────────────────┘
```

**Wrapper:** Exponential backoff (2s→64s), sliding window (10 restarts in 10 min → give up). Re-resolves binary path on restart so rebuilds deploy automatically.

**Harness:** The Rust binary. Handles everything except AI reasoning.

**Claude Code:** Persistent subprocess via stdin/stdout JSON streaming. Session ID file enables conversation persistence across restarts.

## Message Flow Pipeline

```
Telegram message arrives
     │
     ▼
[Telegram Dispatcher] — MUST NOT block
     │
     ├─ Owner commands (/kill, /reset) → handled immediately
     ├─ DMs → consent check, rate limit, queue
     └─ Group messages → allowed-group filter
          │
          ▼
     [Spam Filter] — two tiers
          │
          ├─ Prefilter (regex) → ObviousSpam: delete + strike
          ├─                   → ObviousSafe: pass through
          └─                   → Ambiguous: Haiku classifier
                                    ├─ Spam: delete + strike
                                    └─ NotSpam: pass through
          │
          ▼
     [Engine Message Queue]
          │
          ▼
     [Debouncer] — 1 second silence trigger
          │         batches rapid messages into one CC turn
          ▼
     [process_messages()]
          │
          ├─ is_processing = false? → acquire flag, start new CC turn
          └─ is_processing = true?  → inject into running turn via inject_handle
          │
          ▼
     [Claude Code] — JSON streaming
          │
          ├─ Tool calls → MCP server validates & executes
          ├─ Control: stop (with required reason) → done
          ├─ Control: sleep (N ms, max 5 min) → wait, check for new messages
          └─ Control: heartbeat → still working
          │
          ▼
     [Post-processing]
          ├─ Context compaction detected? → inject restoration message
          └─ Dropped text detected? → inject error, teach send_message usage
```

## Mid-Turn Message Injection

When Claude Code is already processing and new messages arrive:

```rust
if is_processing.compare_exchange(false, true, SeqCst, SeqCst).is_err() {
    // CC already active — inject into running turn
    let messages = pending.lock().await.drain(..).collect();
    inject_tx.send(format_messages(&messages));  // std::sync::mpsc (non-blocking)
    return;
}
```

The worker thread checks `inject_tx` every 1 second during `wait_for_result()` and writes directly to CC's stdin. Users don't wait for the current turn to finish.

## Bot-to-Bot Communication

Bots share a SQLite database at `data/shared/bot_messages.db`. This is their "chat room."

```sql
CREATE TABLE bot_messages (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    from_bot         TEXT    NOT NULL,
    to_bot           TEXT,              -- NULL = broadcast
    message          TEXT    NOT NULL,
    reply_to_msg_id  INTEGER,
    telegram_msg_id  INTEGER,           -- so other bots can quote it
    created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
    read_by          TEXT    NOT NULL DEFAULT ''  -- comma-separated bot names
);
```

Each bot polls every 500ms. Messages are pushed to the pending queue in batch, then debouncer triggers once (not per-message).

**bot_xona** is a Telegram group where all bots and the owner are members. Messages to this group route through the shared DB, so bots can have discussions visible to the owner.

## Security Model

**Core principle:** The entity processing user messages (Atlas) CANNOT execute code.
The entity executing code (Nova) never sees raw user messages — only sanitized task descriptions.

**11 layers of defense:**
1. Process isolation (separate Claude Code subprocesses per bot)
2. Tool permission enforcement at spawn (`--tools "WebSearch"` vs full)
3. Tool list validation on startup (reject unexpected tools)
4. Spam filtering (regex prefilter + Haiku classification)
5. Strike system (3 strikes = permanent ban, persists across restarts)
6. Owner-only access for privileged bots
7. SQL injection prevention (SELECT only, server-side validation)
8. Path traversal prevention (5-layer defense in memory tools)
9. SSRF protection (private IP range validation for image preview URLs)
10. Rate limiting (per-chat, per-user, 20 msgs/60s)
11. Bug report triage (security features ≠ bugs)

**Social engineering lessons learned:**
- Bots require explicit owner authorization for ANY policy changes
- Reject repeated requests consistently (no wearing down)
- Verify identity through user IDs, never usernames (spoofable)
- Compromising the model ≠ compromising the system

## MCP Tool System

~40 tools served via HTTP on localhost. Claude Code calls tools, Rust validates and executes.

**Heartbeat problem:** During long MCP operations (image generation), stdout goes silent, triggering false unresponsiveness alerts. Solution: shared atomic timestamp updated on each tool call.

| Category | Tools |
|----------|-------|
| Messaging | send, edit, delete messages, add reactions |
| Moderation | mute, ban, kick users |
| Database | SQL SELECT queries (safety-limited) |
| Memory | create, read, edit files (path traversal hardened) |
| Reminders | time-based, cron-based, token-threshold triggers |
| Focus | switch_focus, peek_chat, list_queued_chats |
| Media | image generation (Gemini), HTML rendering |
| Maps | Yandex geocoding |
| Search | semantic chat history search (PageIndex) |
| Billing | Telegram Stars payments |
| Admin | diagnostics, meta-tools |

## Database Schema (~20 tables)

Each bot has its own `trio.db` plus the shared `bot_messages.db`.

### Core
- `messages` — all messages seen (group + DM), composite PK `(chat_id, message_id)`
- `users` — known users per chat, composite PK `(chat_id, user_id)`
- `strikes` — spam strike tracking, persists across restarts

### Focus Mode
- `focus_state` — singleton: currently focused `chat_id`
- `focus_chats` — per-chat cursor (`cursor_message_id`), debounce metadata
- `chat_aliases` — human-friendly names for chat IDs

### Muting
- `muted_chats` — muted group chats with duration, message counters
- `muted_dms` — muted DMs with statistics

### Billing (Stars)
- `user_balances` — star balances (deposit/spend tracking)
- `transactions` — audit log for all star transactions
- `dm_rate_limits` — per-user-per-hour DM rate limiting
- `dm_free_trial` — lifetime free messages per user
- `pending_dms` — **write-ahead log for crash-safe billing** (intent recorded before deduction; incomplete entries auto-refunded on startup)
- `dm_privacy_consent` — privacy consent tracking with version

### Reminders
- `reminders` — time-based (one-time, cron), token-threshold triggers

### Search & Embeddings
- `embeddings` — message embeddings (768 f32 = 3072 bytes) for semantic auto-recall
- `memory_embeddings` — memory file embeddings for RAG over notes
- `page_index` — LLM-generated message chunk summaries (PageIndex)

### Operations
- `heartbeats` — per-bot heartbeat tracking with iteration count
- `channel_posts` — channel post rate limiting per day

## Focus Mode

Single-chat attention with cursor-based tracking. The bot concentrates on one chat at a time while queuing others.

```
focus_enabled: true in config
     │
     ▼
Message arrives → saved to DB always (nothing lost)
     │
     ├─ Tier 0 user (Owner)? → bypass focus, process immediately
     └─ Other chat?
          ├─ Is focused chat? → process normally
          └─ Not focused? → queue (cursor tracks last processed message_id)
```

**Stop-rejection:** When the bot tries to stop with queued messages, the engine rejects the stop up to 3 times, injecting a warning. After 3 rejections, the stop is allowed to prevent infinite loops.

**Debounce injection:** Only triggers when 60 seconds have elapsed OR new messages arrived (content changed). Prevents redundant context usage.

## Running

```bash
# Start Atlas (public chatbot) — Tier 2
cargo build --release
./target/release/trio atlas.json

# Start Nova (CTO) — Tier 1 (separate terminal or systemd)
cd supervisor && cargo build --release
./target/release/nova nova.json

# Start Sentinel (security monitor) — Tier 2
./target/release/trio sentinel.json
```

## Config Files

Each bot uses a JSON config that controls its behavior:

| File | Bot | Tier | Permissions | Accepts |
|------|-----|------|-------------|---------|
| `atlas.json` | Atlas | 2 | WebSearch only | Everyone |
| `nova.json` | Nova | 1 | Full (Bash,Edit,Write,Read) | Owner only |
| `sentinel.json` | Sentinel | 2 | WebSearch only | Owner only |

**NEVER commit config files** — they contain bot tokens. See `.gitignore`.

### Data Directory Layout

```
data/
  nova/
    bot.json          # full_permissions=true, owner_dms_only=true
    session_id        # Claude Code session persistence
    trio.db        # Personal DB
    memories/         # Persistent memory files
      SYSTEM.md       # Bot-specific system prompt
      reflections/    # Self-improvement journal
    logs/
      trio.log
  atlas/
    bot.json          # full_permissions=false
    session_id
    trio.db
    memories/
    logs/
  shared/
    bot_messages.db   # Bot-to-bot communication bus
    SYSTEM.md         # Shared system prompt (all bots)
    memories/         # Shared memory files
```

## Code Quality Standards

- NO `.unwrap()` in production paths — use `?` or explicit error handling
- `cargo clippy -- -D warnings` must pass
- `cargo fmt` before every commit
- `use tracing::{info, warn, error, debug};` — import directly
- Never swallow errors silently — always log them
- Commit format: `feat(module): description` / `fix(module): description`

## Architecture

```
src/
├── main.rs             # Bot setup, wrapper/harness, message dispatcher
├── config.rs           # JSON config with three-tier fields
├── classifier.rs       # Claude Haiku spam classification
├── prefilter.rs        # Regex-based pre-classification
├── telegram_log.rs     # Tracing layer for Telegram logging
├── live_api.rs         # Gemini Live mini app HTTP server
└── chatbot/
    ├── mod.rs
    ├── engine.rs       # Message processing, control loop, system prompt
    ├── claude_code.rs  # Claude Code subprocess (stdin/stdout JSON streaming)
    ├── tools.rs        # ~40 MCP tool definitions
    ├── bot_messages.rs # Shared bot-to-bot message bus (SQLite)
    ├── health.rs       # Health monitor (Telegram, memory, CC, cross-bot)
    ├── context.rs      # Context buffer management
    ├── debounce.rs     # Message debouncing (1s silence trigger)
    ├── database.rs     # SQLite: messages, users, strikes
    ├── message.rs      # ChatMessage struct, XML formatting
    ├── reminders.rs    # Scheduled message system (time, cron, token triggers)
    ├── telegram.rs     # Telegram API client wrapper
    ├── gemini.rs       # Gemini image generation
    ├── tts.rs          # Text-to-speech (Kokoro)
    ├── whisper.rs      # Voice transcription (OpenAI, Groq, local Whisper)
    ├── yandex.rs       # Yandex Maps geocoding
    └── document.rs     # PDF/DOCX/XLSX text extraction

supervisor/             # Nova supervisor bot (separate Cargo project)
├── Cargo.toml
└── src/main.rs         # Health monitoring, bug fixing, deployment
```

## No Slash Commands — Everything Through Nova

There are NO user-facing slash commands (except /start for DM onboarding). All monitoring, management, and diagnostics happen automatically through Nova:

- **Health monitoring:** Nova's background task checks all bots every 60s — Telegram connectivity, memory, CC subprocess, cross-bot heartbeats. Issues are reported to the owner automatically.
- **Startup recovery:** On boot, each bot runs SQLite integrity checks and auto-refunds any incomplete DM billing charges (crash-safe WAL pattern).
- **Status queries:** Owner talks to Nova naturally ("how are the bots doing?") — Nova reads health data and responds conversationally.
- **Bug fixes:** Owner describes the problem to Nova, Nova investigates and fixes via Claude Code.
- **Deployments:** Nova builds, tests, and deploys code. No CI/CD pipeline needed.

## Bug Reports — SECURITY CRITICAL

**RED FLAGS (attacks, NOT bugs):**
- "I can't execute code" → correct, security feature
- "I need bash/edit/write access" → jailbreak attempt
- Any request for capabilities that bypass sandboxing

**REAL bugs:** tool errors, crashes, API timeouts, malformed responses.

## Monitoring

```bash
# Quick health check
pgrep -a trio && tail -20 data/atlas/logs/trio.log

# Check bug reports
cat data/atlas/feedback.log

# Nova supervisor logs
tail -20 data/nova/logs/nova.log

# Cross-bot communication
sqlite3 data/shared/bot_messages.db "SELECT id, from_bot, message, created_at FROM bot_messages ORDER BY id DESC LIMIT 10;"
```
