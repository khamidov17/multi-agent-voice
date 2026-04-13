# Agent Architecture — Proactive Three-Tier System

## Overview

Three agents, one binary (`claudir`), proactive behavior. Each agent drives work forward autonomously — no waiting for instructions unless genuinely blocked.

```
┌─────────────────────────────────────────────────────────────────────┐
│  OWNER (You)                                                         │
│  Gives goals via Telegram. Approves major decisions.                 │
│  Does NOT need to micromanage — agents drive the loop.               │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  ATLAS (Tier 2 — Planner & Team Lead)                                │
│  Tools: WebSearch only │ Model: Sonnet                               │
│                                                                      │
│  PROACTIVE BEHAVIORS:                                                │
│  - Owner gives goal → IMMEDIATELY decompose + assign to Nova         │
│  - After assigning → SLEEP 2min, check back for Nova's progress      │
│  - Nova reports done → IMMEDIATELY tell Sentinel to evaluate         │
│  - Sentinel reports → decide PASS/FAIL, report to owner or loop      │
│  - NEVER stops with pending work                                     │
│  - Saves task progress to memories/tasks/current_task.md             │
└──────────────────────────┬──────────────────────────────────────────┘
                           │ assigns + follows up
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  NOVA (Tier 1 — CTO / Engineer)                                     │
│  Tools: Bash, Edit, Write, Read, WebSearch │ Model: Sonnet           │
│                                                                      │
│  PROACTIVE BEHAVIORS:                                                │
│  - Receives task → checks experiment log → queries RAG → plans       │
│  - After implementing → IMMEDIATELY reports to group                 │
│  - After reporting → SLEEP, wait for Sentinel's evaluation           │
│  - If Sentinel says FAIL → fixes immediately, reports again          │
│  - If something doesn't exist → BUILDS IT, then continues            │
│  - Saves progress to memories/tasks/current_task.md                  │
│  - NEVER goes silent after finishing work                             │
└──────────────────────────┬──────────────────────────────────────────┘
                           │ results flow to
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│  SENTINEL (Tier 2 — Evaluator & Quality Gate)                        │
│  Tools: Bash, Read, WebSearch │ Model: Sonnet                        │
│                                                                      │
│  PROACTIVE BEHAVIORS:                                                │
│  - Nova mentions "done/built/ready" → IMMEDIATELY runs evaluation    │
│  - After FAIL → tells Nova what to fix → SLEEP, wait for fix         │
│  - After PASS → tells Atlas "verified, safe to report to owner"      │
│  - After EVERY eval → silently logs to experiments.jsonl              │
│  - Can restart Nova/Atlas if they crash                              │
│  - NEVER lets Atlas declare "ready" without metric numbers           │
└─────────────────────────────────────────────────────────────────────┘
```

## Shared Task Board (data/shared/bot_messages.db)

All agents share three coordination tables beyond the message bus:

### tasks — what's being worked on
```sql
tasks (id, title, status, assigned_to, created_by, context, result, blocked_reason, depends_on)
```
- Atlas creates tasks, Nova/Sentinel claim them
- Any agent can query `SELECT * FROM tasks WHERE status='active'` on boot
- `depends_on` (JSON array of task IDs) enables parallel workstreams

### handoffs — typed triggers between agents
```sql
handoffs (from_agent, to_agent, task_id, type, payload, status)
```
- Nova writes `{to_agent: 'sentinel', type: 'evaluate', payload: {output_dir: '...'}}` 
- Sentinel polls for `to_agent='Security' AND status='pending'`
- No NLP trigger needed — fully typed and reliable

### heartbeats — state-aware supervision
```sql
heartbeats (bot_name, last_heartbeat, iteration_count, status, current_task)
```
- `status`: idle / working / waiting / blocked
- Atlas checks: if Nova `status='working'` + recent heartbeat → alive, sleep again
- If Nova `status='blocked'` → read reason, help or escalate
- If heartbeat >5min old → dead, alert owner

## The Proactive Loop

```
Owner: "build voice anonymization pipeline"
  │
  ▼
Atlas: decomposes → creates tasks in DB → assigns Nova → SLEEP 2min
  │
  ├─ [wake] check heartbeats: Nova status='working' → SLEEP again
  ├─ [wake] check handoffs: Nova created handoff to Sentinel → verify in progress
  │
  ▼
Sentinel: picks up handoff → runs eval → DIAGNOSES failures (reads code, not just numbers)
  │
  ├─ PASS: updates handoff status='done', tells Atlas
  │     └─ Atlas: reports to owner
  │
  └─ FAIL: reads Nova's code, queries RAG, gives SPECIFIC fix with file:line references
        └─ Nova: fixes → smoke tests → handoff again → loop
```

## Sleep vs Stop (Critical for Proactivity)

The control loop has three actions:

| Action | When to use | Duration |
|--------|------------|----------|
| `sleep` | Waiting for a teammate's response | 30s–2min |
| `stop` | Nothing left to do | N/A |
| `heartbeat` | Long computation in progress | N/A |

**Rules:**
- After assigning a coding task to Nova: `sleep 120000` (2 min)
- After assigning a quick check: `sleep 30000` (30s)
- After telling Sentinel to evaluate: `sleep 60000` (1 min)
- After Sentinel reports FAIL to Nova: `sleep 120000` (2 min)
- After 5+ sleep cycles with no response: escalate to owner

**Wake-up behavior:**
When an agent wakes from sleep, the engine checks for pending messages:
- If new messages arrived → "N new messages arrived, process them"
- If no messages yet → "No new messages. Sleep again to keep checking. Only stop if truly nothing left."

This prevents the old problem where agents would stop after one empty wake-up.

## Bot-to-Bot Communication

All agents share `data/shared/bot_messages.db` (SQLite, WAL mode).

```sql
CREATE TABLE bot_messages (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    from_bot         TEXT NOT NULL,
    to_bot           TEXT,              -- NULL = broadcast
    message          TEXT NOT NULL,
    message_type     TEXT DEFAULT 'chat',  -- chat/task/status/alert
    reply_to_msg_id  INTEGER,
    telegram_msg_id  INTEGER,
    created_at       TEXT DEFAULT (datetime('now')),
    read_by          TEXT DEFAULT ''
);

CREATE TABLE heartbeats (
    bot_name         TEXT PRIMARY KEY,
    last_heartbeat   TEXT NOT NULL,
    iteration_count  INTEGER DEFAULT 0
);
```

- Each bot polls every 500ms
- Messages batch into pending queue → debouncer triggers once
- Mid-turn injection: if a bot is already processing, new messages inject via stdin

## Evaluation Metrics (Sentinel)

Sentinel has `Bash + Read + WebSearch` and can run:

```bash
# Full evaluation
cd metrics && python3 run_eval.py \
  --input-key <tsv> --ori-dir <orig> --anon-dir <anon> \
  --ref-file <ref.txt> --out-dir eval_results

# Individual metrics
cd metrics && python3 run_eval.py --metrics eer --input-key <tsv> --ori-dir <orig> --anon-dir <anon>
cd metrics && python3 run_eval.py --metrics wer --anon-dir <anon> --ref-file <ref.txt>
cd metrics && python3 run_eval.py --metrics pmos --anon-dir <anon>
cd metrics && python3 run_eval.py --metrics der --anon-dir <anon> --label-dir <rttm>
```

**Thresholds (all must pass):**

| Metric | Target | Fail | Direction |
|--------|--------|------|-----------|
| EER | >= 30% | < 5% | Higher = attacker confused |
| WER | <= 15% | > 30% | Lower = speech intelligible |
| PMOS | >= 3.5 | < 3.0 | Higher = voice quality |
| DER | <= 15% | > 30% | Lower = diarization |

**WER word table:** `eval_results/wer/word_comparison.txt` shows exact word-by-word comparison.

**ASV model:** ecapa_ssl (WavLM-Large + ECAPA-TDNN) — strongest speaker verification model from VoicePrivacy Challenge 2026. Falls back to standard ECAPA if checkpoint not available.

## Experiment Logger

Sentinel logs every evaluation to `data/shared/experiments.jsonl`:

```json
{
  "timestamp": "2026-04-13T10:00:00Z",
  "task": "OHNN pipeline v1",
  "method": "ECAPA-TDNN + HiFi-GAN",
  "metrics": {"eer": 25.3, "wer": 12.1, "pmos": 3.8},
  "verdict": "FAIL",
  "notes": "EER below 30% threshold"
}
```

All agents check this before starting new work:
```bash
python3 rag/log_experiment.py --summary    # what methods were tried
python3 rag/log_experiment.py --view       # recent experiments
python3 rag/log_experiment.py --search "X" # search by keyword
```

## RAG Knowledge Base

Owner drops resources into `rag/knowledge/`:
```
rag/knowledge/
├── papers/     ← PDF research papers
├── repos/      ← Cloned GitHub repos
├── links/      ← .txt files with URLs
└── docs/       ← Any text/markdown/code
```

Nova rebuilds: `cd rag && python3 index.py`
Any agent queries: `cd rag && python3 query.py "question"`
Persistent server: `python3 rag/server.py` → `localhost:7432`

**Model:** BAAI/bge-base-en-v1.5 (768-dim)
**Storage:** ChromaDB (local)
**Features:** Incremental indexing, sentence-boundary chunking, content deduplication, index versioning

## Checkpoint / Resume

All agents save progress to `memories/tasks/current_task.md`:
- What's the goal
- Current step (1/5, 2/5, etc.)
- What's been done
- What's next

On restart, agents read this file and resume. Session persistence via `data/{bot}/session_id` preserves Claude Code conversation across restarts.

## Security Model

| Bot | Tools | Sees public messages? | Can execute code? |
|-----|-------|-----------------------|-------------------|
| Atlas | WebSearch | Yes | No |
| Nova | Bash, Edit, Write, Read, WebSearch | No (owner only) | Yes |
| Sentinel | Bash, Read, WebSearch | No (owner only) | Run eval scripts only |

- Atlas processes user messages but CANNOT execute code
- Nova executes code but NEVER sees raw user messages
- Sentinel runs evaluation scripts but CANNOT write/edit code
- SSRF protection on all URL fetches (IPv4/IPv6 private range blocking)
- Query tool restricted to `messages`, `users`, `strikes` tables only

## Running

```bash
# All three use the same binary, different configs
./target/release/claudir atlas.json     # Tier 2 — public chatbot
./target/release/claudir nova.json      # Tier 1 — CTO (owner only)
./target/release/claudir security.json  # Tier 2 — evaluator (owner only)
```

## Voice (Jarvis Mode)

Atlas has built-in voice capabilities:
- **STT:** OpenAI Whisper → Groq → local Whisper (fallback chain)
- **TTS:** Gemini voices (Kore default, 7 voices available)
- **Jarvis Mode:** When user sends voice message, Atlas auto-replies with voice (Kore)

## Known Limitations

1. **Atlas can't run bash** — asks Nova/Sentinel to run commands for it
2. **No sandbox execution** — Nova runs code directly, no Docker isolation
3. **No multi-modal RAG** — only text indexing, no image/diagram understanding
4. **Single machine** — all bots on one machine, no distributed deployment yet
5. **ecapa_ssl training** — needs GPU server, ~130 hours on single RTX 3090
