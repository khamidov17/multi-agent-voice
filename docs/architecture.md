# Claudir Architecture

Three bots, three trust levels, one Rust binary. Owner controls everything via Telegram. Each bot instance runs as three OS processes: wrapper (crash recovery), harness (Rust, Telegram I/O, MCP tools), and Claude Code (AI reasoning subprocess).

---

## 1. System Overview

### 1.1 The Three Tiers

```
 TIER 0: Owner & Supervisor
 +-----------------------------------------------------------------+
 | Owner communicates via Telegram (bot_xona group or DMs to Nova) |
 | Supervisor = raw `claude` CLI on the server (emergency fallback)|
 +-----------------------------------------------------------------+

 TIER 1: Nova (CTO / Private Assistant)
 +-----------------------------------------------------------------+
 | full_permissions: true  |  owner_dms_only: true                 |
 | Claude Code tools: Bash, Edit, Write, Read, WebSearch           |
 | Model: claude-opus-4-6                                          |
 |                                                                 |
 | Manages code, deploys updates, monitors Atlas & Sentinel,       |
 | acts as owner's proxy. Only talks to owner and other bots.      |
 | --dangerously-skip-permissions (safe: owner-only access)        |
 +-----------------------------------------------------------------+
        |
        | monitors & coordinates
        v
 TIER 2: Atlas (Public Chatbot)          Sentinel (Security Monitor)
 +-------------------------------+  +-------------------------------+
 | full_permissions: false       |  | full_permissions: false       |
 | owner_dms_only: false         |  | owner_dms_only: true          |
 | Tools: WebSearch, WebFetch    |  | Tools: WebSearch only         |
 | CANNOT execute code           |  | Security auditing, vuln scans |
 | ~40 MCP tools for Telegram    |  | Log analysis, monitoring      |
 | Handles groups + DMs          |  | Owner-only communication      |
 +-------------------------------+  +-------------------------------+
```

**Core security invariant:** Atlas processes user messages but cannot execute code. Nova executes code but never sees raw user messages -- only sanitized task descriptions.

### 1.2 Three-Process Model

Every bot instance runs as three nested OS processes:

```
 +-------------+      +------------------------+      +-------------------+
 |   Wrapper   | ---> |   Harness (claudir)    | ---> |   Claude Code     |
 |             |      |                        |      |                   |
 | Crash       |      | - Telegram dispatcher  |      | - AI reasoning    |
 | recovery    |      | - MCP tool server      |      | - stdin/stdout    |
 | loop        |      | - Spam filtering       |      |   JSON streaming  |
 |             |      | - Debouncer            |      | - Session         |
 | Exp backoff |      | - Message queue        |      |   persistence     |
 | 2s -> 64s   |      | - Health monitor       |      | - Structured      |
 |             |      | - Bot-to-bot bus       |      |   output schema   |
 +-------------+      +------------------------+      +-------------------+
```

**Wrapper** (`run_wrapper()` in `main.rs`):
- Sliding window crash detection: 10 restarts in 10 minutes = give up
- Exponential backoff: 2s, 4s, 8s, 16s, 32s, 64s for rapid crashes (< 10s runtime)
- Re-resolves binary path on every restart, so `cargo build --release` deploys automatically
- Kill marker file for clean shutdown via `/kill` command
- RAII `ChildGuard` ensures `wait()` is always called, preventing zombie processes

**Harness** (`main.rs` + `chatbot/` modules):
- The Rust binary. Handles everything except AI reasoning
- Spawns Claude Code as a persistent subprocess
- Runs Telegram polling, spam filtering, MCP tool execution, health monitoring

**Claude Code** (`chatbot/claude_code.rs`):
- Persistent subprocess communicating via stdin/stdout JSON streaming
- Session ID file enables conversation persistence across restarts (`--resume`)
- Structured output schema with control actions: `stop`, `sleep`, `heartbeat`
- Worker thread runs on a `std::thread` (not tokio) to avoid blocking the async runtime

---

## 2. Message Flow Pipeline

```
Telegram update arrives
      |
      v
[Telegram Dispatcher] ---- MUST NOT block the update loop
      |
      +-- Owner commands (/kill, /reset, /start) --> handled immediately
      |
      +-- DMs --> consent check --> rate limit (50/hr free) --> queue
      |
      +-- Group messages --> allowed_groups filter
             |
             v
        [Spam Filter] ---- two tiers
             |
             +-- Prefilter (regex) --> ObviousSpam: delete + strike
             |                     --> ObviousSafe: pass through
             |                     --> Ambiguous: send to Haiku classifier
             |                            +-- Spam: delete + strike
             |                            +-- NotSpam: pass through
             v
        [Engine: handle_message()]
             |
             +-- Store in ContextBuffer and Database
             +-- Push to pending queue (Vec<ChatMessage>)
             +-- Trigger debouncer
             |
             v
        [Debouncer] ---- 1-second silence trigger
             |             Batches rapid messages into one CC turn
             |             Uses tokio::select! with reset channel
             v
        [Debouncer callback]
             |
             +-- is_processing == false?
             |     +-- compare_exchange(false, true)
             |     +-- Drain pending queue
             |     +-- Spawn tokio task: process_messages()
             |
             +-- is_processing == true?
                   +-- Mid-turn injection via inject_handle
                   +-- Messages written directly to CC stdin
                   +-- No waiting for current turn to finish
             |
             v
        [process_messages()] ---- tool call loop
             |
             +-- Format messages as XML, send to Claude Code
             +-- Auto-inject user memory for DM conversations
             +-- Send typing indicator to all pending chats
             |
             +-- LOOP (max 10 iterations Tier 2, 30 Tier 1):
             |     +-- Parse structured output: action + tool_calls
             |     +-- action == "heartbeat" --> acknowledge, continue
             |     +-- action == "sleep"     --> pause, check new msgs
             |     +-- action == "stop"      --> stop rejection check
             |     +-- Execute tool calls via MCP
             |     +-- Send results back to Claude Code
             |
             +-- Timeout: 120s (Tier 2) / 600s (Tier 1)
             |     On timeout: reset CC subprocess, notify user
             |
             v
        [Post-processing]
             +-- Save context and database state
             +-- Set is_processing = false
             +-- Check for autonomous task continuation (see Section 4)
             +-- Compaction detection --> stop cleanly to avoid runaway
```

### 2.1 Mid-Turn Message Injection

When Claude Code is already processing and new messages arrive, they are injected into the running turn without waiting:

```rust
// In debouncer callback when is_processing == true:
let messages = pending.lock().await.drain(..).collect();
let formatted = format_messages_continuation(&messages);
inject_handle.lock().send(formatted);  // std::sync::mpsc (non-blocking)
```

The worker thread checks the inject channel every 1 second during `wait_for_result()` and writes directly to CC's stdin. Owner messages get a `[PRIORITY]` prefix.

### 2.2 Stop Rejection

When Claude Code tries to stop but new messages arrived during processing:

1. Engine checks the pending queue
2. If non-empty and rejections < 3, inject a warning message back to CC
3. CC must handle the new messages before stopping
4. After 3 rejections, the stop is allowed to prevent infinite loops

---

## 3. Bot-to-Bot Communication

Telegram bots cannot see messages from other bots in groups (API limitation). Claudir works around this with a shared SQLite database.

### 3.1 Shared Database Bus

All bots share `data/shared/bot_messages.db` with WAL mode for concurrent access:

```sql
CREATE TABLE bot_messages (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    from_bot         TEXT    NOT NULL,
    to_bot           TEXT,              -- NULL = broadcast to all bots
    message          TEXT    NOT NULL,
    message_type     TEXT    NOT NULL DEFAULT 'chat',
    reply_to_msg_id  INTEGER,
    telegram_msg_id  INTEGER,          -- for quoting in Telegram
    created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
    read_by          TEXT    NOT NULL DEFAULT ''  -- comma-separated bot names
);

CREATE TABLE heartbeats (
    bot_name         TEXT    PRIMARY KEY,
    last_heartbeat   TEXT    NOT NULL,
    iteration_count  INTEGER NOT NULL DEFAULT 0
);
```

### 3.2 Message Types

Structured routing via `message_type`:

| Type | Purpose |
|------|---------|
| `chat` | Normal conversation (default for group messages) |
| `task` | Task assignment from Nova to other bots |
| `status` | Progress update |
| `alert` | Critical issue (health, security) |

### 3.3 Polling Loop

`start_polling()` in `bot_messages.rs` spawns a background task:

```
Every 500ms:
  1. Query unread_for(this_bot) -- messages not in read_by
  2. Filter client-side (avoids SQL LIKE edge cases)
  3. Push ALL messages to pending queue in one batch
  4. Mark each message as read
  5. Trigger debouncer ONCE (not per-message)
```

Messages are pushed to pending as `ChatMessage` structs with synthetic user IDs mapped from bot names (`Atlas -> 8446778880`, `Nova -> 8338468521`, `Security -> 8373868633`).

### 3.4 bot_xona Group

The `bot_xona` Telegram group (`-1003399442526`) contains all bots and the owner. Messages to this group route through the shared DB, enabling visible multi-bot discussions.

---

## 4. Autonomous Task Continuation

After Claude Code sends a `stop` action and the processing turn ends, the engine checks for pending tasks that need autonomous continuation. This prevents multi-step tasks from stalling.

### 4.1 The Problem

Without this feature, when Nova assigns a task to Atlas via the bot-to-bot bus, Atlas processes it, stops, and then... nothing. If the task requires multiple turns (e.g., checking something, then acting on the result), there is no trigger for the next turn.

### 4.2 The Notify Pattern

```
process_messages() completes
      |
      v
is_processing = false
      |
      v
Check shared_bot_messages_db for pending_tasks_for(this_bot)
      |
      +-- No pending tasks? --> check if new messages arrived --> done
      |
      +-- Pending tasks found?
            |
            +-- Create synthetic TASK_CONTINUE ChatMessage:
            |     "[SYSTEM] TASK_CONTINUE: you have N pending task(s).
            |      Next task from <bot>: <message>"
            |
            +-- Push to pending queue
            +-- Wait 100ms (let is_processing=false settle)
            +-- retrigger.notify_one()
                    |
                    v
            [Watcher task] (spawned in start_debouncer)
                    |
                    +-- Awaits retrigger.notified()
                    +-- Calls debouncer.trigger()
                    +-- New processing turn begins
```

The `retrigger` is a `tokio::sync::Notify` shared between the debouncer callback and a dedicated watcher task. This avoids the debouncer callback needing to call itself (which would be a deadlock).

---

## 5. Security Model

**Core principle:** The entity processing user messages (Atlas) CANNOT execute code. The entity executing code (Nova) never sees raw user messages.

### 5.1 Eleven Layers of Defense

| # | Layer | Implementation |
|---|-------|----------------|
| 1 | Process isolation | Separate Claude Code subprocesses per bot |
| 2 | Tool permission enforcement | `--tools "WebSearch"` vs full at spawn time |
| 3 | Tool list validation | Reject unexpected tools on startup |
| 4 | Spam filtering | Regex prefilter + Claude Haiku classifier |
| 5 | Strike system | 3 strikes = permanent ban, persists in SQLite |
| 6 | Owner-only access | `owner_dms_only` for privileged bots |
| 7 | SQL injection prevention | SELECT-only, server-side validation in `query` tool |
| 8 | Path traversal prevention | 5-layer defense in memory tools |
| 9 | SSRF protection | Private IP range validation for image URLs |
| 10 | Rate limiting | Per-chat, per-user, 50 msgs/60 min (free tier) |
| 11 | Bug report triage | Security features != bugs; red flag detection |

### 5.2 Spam Filtering Pipeline

```
Message text
    |
    v
[Prefilter] -- compiled regex patterns from config
    |
    +-- Matches spam_patterns? --> ObviousSpam
    +-- Matches safe_patterns? --> ObviousSafe
    +-- Neither?               --> Ambiguous
    |
    v
[Classifier] -- Claude Haiku API call (Ambiguous only)
    |
    +-- Spam    --> delete message + add strike
    +-- NotSpam --> pass through to engine
```

Default spam patterns match crypto scams, t.me links, "DM me for" messages. Default safe patterns match non-Latin text, short words, and greetings.

### 5.3 Social Engineering Defenses

- Bots require explicit owner authorization for ANY policy changes
- Reject repeated requests consistently (no wearing down)
- Verify identity through user IDs, never usernames (spoofable)
- Compromising the model != compromising the system

---

## 6. MCP Tool System

~40 tools served via structured output. Claude Code emits JSON with `tool_calls` arrays, and the Rust harness validates and executes them.

### 6.1 Tool Categories

| Category | Tools | Notes |
|----------|-------|-------|
| Messaging | `send_message`, `edit_message`, `delete_message`, `add_reaction` | All require `chat_id` |
| Voice | `send_voice` | TTS via Kokoro-FastAPI |
| Moderation | `mute_user`, `ban_user`, `kick_user`, `unban_user` | Owner notified |
| Database | `query` | SELECT only, max 100 rows, text truncated |
| Memory | `create_memory`, `read_memory`, `edit_memory`, `delete_memory`, `list_memories`, `search_memories` | Path traversal hardened |
| Reminders | `set_reminder`, `list_reminders`, `cancel_reminder` | Time-based, cron, token-threshold |
| Media | `send_photo` (Gemini image gen/edit), `send_music`, `send_file` | Source image editing supported |
| Documents | `create_spreadsheet`, `create_pdf`, `create_word` | XLSX, PDF, DOCX generation |
| Maps | `yandex_geocode`, `yandex_map` | Geocoding + static map images |
| Search | `web_search` (Brave API), `fetch_url` | Web content retrieval |
| Polls | `send_poll` | Anonymous/multiple-answer |
| Members | `get_members`, `get_user_info`, `get_chat_admins`, `import_members` | Database-backed |
| Admin | `report_bug`, `now` | Diagnostics |

### 6.2 Structured Output Schema

Claude Code uses a JSON schema with three required components:

```json
{
  "action": "stop|sleep|heartbeat",   // Control action (required)
  "reason": "...",                      // Required when action=stop
  "sleep_ms": 5000,                     // When action=sleep (max 300000)
  "tool_calls": [                       // Optional array of tool calls
    { "tool": "send_message", "chat_id": -123, "text": "Hello" }
  ]
}
```

### 6.3 Heartbeat Problem

During long MCP operations (image generation), stdout goes silent, triggering false unresponsiveness alerts. Solution: shared `AtomicU64` timestamp updated on each tool call, checked by the health monitor independently of stdout.

---

## 7. Database Schema Overview

Each bot has its own `claudir.db` (SQLite) plus the shared `bot_messages.db`.

### 7.1 Core Tables

| Table | Primary Key | Purpose |
|-------|-------------|---------|
| `messages` | `message_id` | All messages seen (group + DM) |
| `users` | `user_id` | Known users with join date, message count, status |
| `strikes` | `user_id` | Spam strike tracking, persists across restarts |

### 7.2 Focus Mode Tables

| Table | Purpose |
|-------|---------|
| `focus_state` | Singleton: currently focused `chat_id` |
| `focus_chats` | Per-chat cursor (`cursor_message_id`), debounce metadata |
| `chat_aliases` | Human-friendly names for chat IDs |

### 7.3 Muting

| Table | Purpose |
|-------|---------|
| `muted_chats` | Muted group chats with duration, message counters |
| `muted_dms` | Muted DMs with statistics |

### 7.4 Billing (Telegram Stars)

| Table | Purpose |
|-------|---------|
| `user_balances` | Star balances (deposit/spend tracking) |
| `transactions` | Audit log for all star transactions |
| `dm_rate_limits` | Per-user-per-hour DM rate limiting |
| `dm_free_trial` | Lifetime free messages per user |
| `pending_dms` | **Write-ahead log** for crash-safe billing |
| `dm_privacy_consent` | Privacy consent tracking with version |

The `pending_dms` table implements a WAL pattern: intent is recorded before deducting stars. On startup, incomplete entries are auto-refunded (`recover_pending_dms()`).

### 7.5 Search and Embeddings

| Table | Purpose |
|-------|---------|
| `embeddings` | Message embeddings (768 f32 = 3072 bytes) for semantic auto-recall |
| `memory_embeddings` | Memory file embeddings for RAG over notes |
| `page_index` | LLM-generated message chunk summaries (PageIndex) |

### 7.6 Operations

| Table | Purpose |
|-------|---------|
| `heartbeats` | Per-bot heartbeat tracking with iteration count (shared DB) |
| `channel_posts` | Channel post rate limiting per day |

---

## 8. Focus Mode

Single-chat attention with cursor-based tracking. The bot concentrates on one chat at a time while queuing others.

```
focus_enabled: true in config
      |
      v
Message arrives --> saved to DB always (nothing lost)
      |
      +-- Owner (Tier 0)? --> bypass focus, process immediately
      |
      +-- Focused chat?   --> process normally
      |
      +-- Other chat?     --> queue
                              cursor_message_id tracks last processed msg
                              Messages accumulate until focus switches
```

**Key behaviors:**

- **Stop rejection for queued chats:** When the bot tries to stop with queued chats, the engine rejects the stop up to 3 times, injecting a warning. After 3 rejections, the stop is allowed.
- **Debounce injection throttle:** Only triggers when 60 seconds have elapsed OR new messages arrived (content changed). Prevents redundant context usage.
- **Chat aliases:** Human-friendly names can be assigned to chat IDs for easier focus switching.
- **Peek:** Bot can peek at queued chats without switching focus.

---

## 9. Health Monitoring

### 9.1 Background Health Monitor

`start_health_monitor()` runs every 60 seconds and checks four systems:

| Check | What | Alert Threshold |
|-------|------|-----------------|
| Telegram API | `getMe` ping | Any failure |
| Memory | RSS via `ps`, total via `sysctl`/`/proc/meminfo` | > 80% of system RAM |
| CC subprocess | PID alive (`kill -0`), heartbeat age | PID dead or heartbeat > 120s stale |
| Cross-bot heartbeat | Latest peer message in shared DB | > 300s since last peer message |

The first tick is skipped to avoid startup alarms. Critical issues are logged; Nova monitors logs and handles them autonomously.

### 9.2 Startup Recovery

On boot, `run_startup_checks()` runs:

1. **SQLite integrity check** -- verifies database consistency
2. **Pending DM recovery** -- auto-refunds any incomplete billing charges from the `pending_dms` WAL table

### 9.3 Status Report

`build_status_report()` produces a human-readable health snapshot:
- Harness PID
- CC subprocess PID, status (alive/dead), heartbeat age
- Memory usage (MB and percentage)
- Peer bot heartbeat table with staleness indicators

---

## 10. Config and Deployment

### 10.1 Config Files

Each bot uses a JSON config file (NEVER committed -- contains tokens):

| File | Bot | Tier | Permissions | Accepts |
|------|-----|------|-------------|---------|
| `atlas.json` | Atlas | 2 | WebSearch only | Everyone |
| `nova.json` | Nova | 1 | Full (Bash,Edit,Write,Read) | Owner only |
| `sentinel.json` | Sentinel | 2 | WebSearch only | Owner only |

Key config fields (`src/config.rs`):

```
owner_ids            -- array of owner Telegram user IDs
telegram_bot_token   -- bot API token
bot_name             -- display name ("Atlas", "Nova", "Security")
full_permissions     -- Tier 1 (true) vs Tier 2 (false)
tools                -- custom tool list override
owner_dms_only       -- restrict to owner DMs + allowed groups
data_dir             -- directory for state files
allowed_groups       -- group chat IDs the bot serves
gemini_api_key       -- for image generation
groq_api_key         -- for speech-to-text (secondary)
openai_api_key       -- for Whisper STT (preferred)
tts_endpoint         -- Kokoro-FastAPI URL
premium_users        -- unlimited messages, no rate limit
whisper_model_path   -- local Whisper model (.bin)
```

### 10.2 Data Directory Layout

```
data/
  nova/
    session_id          # Claude Code session persistence
    claudir.db          # Personal SQLite database
    context.json        # Context buffer state
    reminders.db        # Reminder store
    tos_accepted.json   # ToS acceptance list
    memories/           # Persistent memory files
      SYSTEM.md         # Bot-specific system prompt
      reflections/      # Self-improvement journal
    logs/
      claudir.log
  atlas/
    session_id
    claudir.db
    context.json
    memories/
    logs/
  shared/
    bot_messages.db     # Bot-to-bot communication bus
    SYSTEM.md           # Shared system prompt (all bots)
    memories/           # Shared memory files
```

### 10.3 Running

```bash
# Build
cargo build --release

# Start Atlas (public chatbot) -- Tier 2
./target/release/claudir atlas.json

# Start Nova (CTO) -- Tier 1 (separate binary in supervisor/)
cd supervisor && cargo build --release
./target/release/nova nova.json

# Start Sentinel (security monitor) -- Tier 2
./target/release/claudir sentinel.json
```

Wrapper mode is the default. The binary re-resolves its own path on every restart, so `cargo build --release` deploys automatically without restarting the wrapper.

### 10.4 Source Layout

```
src/
  main.rs                # Bot setup, wrapper/harness, Telegram dispatcher
  config.rs              # JSON config with three-tier fields
  classifier.rs          # Claude Haiku spam classification
  prefilter.rs           # Regex-based pre-classification
  telegram_log.rs        # Tracing layer for Telegram logging
  live_api.rs            # Gemini Live mini app HTTP server
  chatbot/
    mod.rs               # Module re-exports
    engine.rs            # Message processing, control loop, system prompt
    claude_code.rs       # Claude Code subprocess (stdin/stdout JSON streaming)
    tools.rs             # ~40 MCP tool definitions (ToolCall enum)
    bot_messages.rs      # Shared bot-to-bot message bus (SQLite)
    health.rs            # Health monitor (Telegram, memory, CC, cross-bot)
    context.rs           # Context buffer management
    debounce.rs          # Message debouncing (1s silence trigger)
    database.rs          # SQLite: messages, users, strikes, billing, focus
    message.rs           # ChatMessage struct, XML formatting
    reminders.rs         # Scheduled message system (time, cron, token triggers)
    telegram.rs          # Telegram API client wrapper
    gemini.rs            # Gemini image generation
    tts.rs               # Text-to-speech (Kokoro)
    whisper.rs           # Voice transcription (OpenAI, Groq, local Whisper)
    yandex.rs            # Yandex Maps geocoding
    document.rs          # PDF/DOCX/XLSX text extraction

supervisor/              # Nova supervisor bot (separate Cargo project)
  Cargo.toml
  src/main.rs            # Health monitoring, bug fixing, deployment
```
