# Bot-to-Bot Communication Architecture

## The Problem

Telegram bots **cannot see messages from other bots** in groups (API limitation). Three bots (Nova, Atlas, Sentinel) need to coordinate — so we built a shared message bus.

## How It Works

All bots share a single SQLite database at `data/shared/bot_messages.db` using WAL journal mode for concurrent access.

```
┌─────────┐          ┌──────────────────────┐          ┌─────────┐
│  Nova   │──write──▶│  bot_messages.db     │◀──write──│  Atlas  │
│         │◀──poll───│  (WAL mode)          │───poll──▶│         │
└─────────┘          │                      │          └─────────┘
                     │  - bot_messages      │
                     │  - heartbeats        │
┌─────────┐          │                      │
│Sentinel │──write──▶│                      │
│         │◀──poll───│                      │
└─────────┘          └──────────────────────┘
```

Each bot polls every **500ms** for unread messages.

## Message Schema

```sql
CREATE TABLE bot_messages (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    from_bot         TEXT    NOT NULL,          -- "Atlas", "Nova", "Sentinel"
    to_bot           TEXT,                      -- NULL = broadcast to all
    message          TEXT    NOT NULL,
    message_type     TEXT    NOT NULL DEFAULT 'chat',
    reply_to_msg_id  INTEGER,                  -- thread replies
    telegram_msg_id  INTEGER,                  -- links to the Telegram message
    created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
    read_by          TEXT    NOT NULL DEFAULT '' -- comma-separated bot names
);
```

## Message Types

| Type | Purpose | Example |
|------|---------|---------|
| `chat` | Normal conversation (default) | Group discussion in bot_xona |
| `task` | Task assignment (Nova → others) | "Atlas, check user X's billing" |
| `status` | Progress updates | "Build complete, deploying now" |
| `alert` | Critical issues | "Health check failed for Atlas" |

## Read Tracking

Messages use a `read_by` column (comma-separated bot names) instead of per-reader rows. When a bot reads a message, its name is appended:

```
read_by: ""           → bot reads → read_by: "Atlas"
read_by: "Atlas"      → bot reads → read_by: "Atlas,Nova"
```

A bot only sees messages where:
1. It is **not** the sender (`from_bot != this_bot`)
2. It is the **recipient** (`to_bot = this_bot`) or it's a **broadcast** (`to_bot IS NULL`)
3. It has **not yet read** the message (name not in `read_by`)

## Heartbeat System

Separate table for liveness tracking:

```sql
CREATE TABLE heartbeats (
    bot_name         TEXT    PRIMARY KEY,
    last_heartbeat   TEXT    NOT NULL,
    iteration_count  INTEGER NOT NULL DEFAULT 0
);
```

Each bot upserts its heartbeat periodically. Nova's health monitor reads all heartbeats to detect unresponsive bots.

## Integration with the Engine

When the poller finds new messages, they flow into the standard message pipeline:

```
Poller (500ms tick)
  │
  ▼
Unread messages found
  │
  ▼
Push ALL to pending queue (locked)     ← batch, don't trigger per-message
  │
  ▼
Mark each as read in DB
  │
  ▼
Trigger debouncer ONCE                 ← single processing turn for the batch
  │
  ▼
Engine processes as normal ChatMessages
```

Messages are converted to `ChatMessage` structs with:
- `chat_id` = the shared group (`bot_xona`)
- `user_id` = the sender bot's Telegram user ID
- `username` / `first_name` = the bot's name

## The Telegram Bridge: bot_xona

`bot_xona` (-1003399442526) is a Telegram group where all bots and the owner are members. When a bot sends a message to this group via Telegram, it also writes it to `bot_messages.db` — so other bots can see it. This makes the group a visible "chat room" where the owner can watch bot conversations happen in real time.

## Convenience Methods

```rust
db.insert(from, to, message, reply_to, telegram_msg_id)  // chat message
db.send_task(from, to, description)                       // task assignment
db.send_alert(from, alert_message)                        // broadcast alert
db.heartbeat(bot_name)                                    // upsert heartbeat
```

## Key Design Decisions

- **SQLite WAL mode** — concurrent reads from 3 processes without blocking
- **Polling over push** — simple, no IPC complexity, 500ms is fast enough
- **Batch + single trigger** — multiple messages in one poll become one processing turn, not N separate turns
- **Client-side read filtering** — avoids SQL LIKE edge cases with comma-separated names
- **Retry on open** — if the DB file doesn't exist yet (peer bot hasn't started), retries with backoff
