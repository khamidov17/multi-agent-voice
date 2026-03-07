# Claudir

A Telegram bot powered by Claude AI. Combines spam filtering with an AI chat participant.

## Features

**Spam Filtering**
- Two-tier classification: fast regex prefilter + Claude Haiku for ambiguous messages
- Strike system: configurable strikes before auto-ban
- Owner exemption

**Chat Participation**
- Responds when mentioned or replied to
- Can read message history, search the web, add reactions
- Admin tools: mute, kick, ban users; delete messages
- Member tracking: monitors joins/leaves

## Architecture

```
Telegram Message
      │
      ▼
  Prefilter (regex)
      │
      ├─── Obvious spam → delete + strike
      ├─── Obvious safe → pass to chatbot
      └─── Ambiguous → Claude Haiku classification
                            │
                            └─── spam/not spam
      │
      ▼
  Chatbot Engine
      │
      ▼
  Claude Code CLI (Opus)
      │
      ▼
  Tool execution (send_message, etc.)
```

Two Claude backends:
- **Haiku** (direct API) - spam classification
- **Opus** (Claude Code CLI) - chat responses

## Requirements

- Rust 2024 edition
- [Claude Code CLI](https://github.com/anthropics/claude-code) installed and authenticated
- Telegram bot token (from @BotFather)
- Anthropic API key (for spam classification)

### System Dependencies

```bash
# Build dependencies (for whisper-rs)
sudo apt-get install libclang-dev cmake

# Audio processing (for voice messages)
sudo apt-get install ffmpeg

# Python (for TTS)
sudo apt-get install python3-pip
```

### Voice Transcription (Whisper)

Download a Whisper model (ggml format):

```bash
# Recommended: base.en model (~150MB, good balance)
wget https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin

# Or smaller/larger models:
# ggml-tiny.en.bin  (~75MB, faster, less accurate)
# ggml-small.en.bin (~500MB, more accurate)
```

Add to config:
```json
"whisper_model_path": "/path/to/ggml-base.en.bin"
```

### Text-to-Speech (XTTS)

Install Coqui TTS and run the XTTS server for voice message output:

```bash
pip3 install TTS flask
python3 scripts/xtts_server.py --port 8880 --voices-dir data/voices
```

Add to config:
```json
"tts_endpoint": "http://localhost:8880"
```

Voice cloning: place `.wav` reference files in `data/voices/` (e.g., `myvoice.wav`).
Then use `reference_id: "myvoice"` in TTS requests.

## Setup

1. Copy the example config:
   ```bash
   cp claudir.example.json claudir.json
   ```

2. Edit `claudir.json` with your credentials:
   - `telegram_bot_token` - from @BotFather
   - `anthropic_api_key` - from Anthropic console
   - `owner_ids` - your Telegram user ID(s)
   - `allowed_groups` - group chat IDs to monitor
   - `whisper_model_path` - path to Whisper model (optional, for voice input)
   - `tts_endpoint` - XTTS URL (optional, for voice output)

3. Build and run:
   ```bash
   cargo build --release
   ./target/release/claudir claudir.json
   ```

## Config Options

| Field | Description |
|-------|-------------|
| `telegram_bot_token` | Bot token from @BotFather |
| `anthropic_api_key` | Anthropic API key for spam classification |
| `gemini_api_key` | Gemini API key for image generation |
| `owner_ids` | User IDs exempt from spam filtering |
| `allowed_groups` | Group IDs to monitor (empty = disabled) |
| `trusted_channels` | Channel IDs for forwarded message trust |
| `max_strikes` | Strikes before ban (default: 3) |
| `dry_run` | Log actions without executing |
| `log_chat_id` | Chat ID for log forwarding |
| `data_dir` | Directory for persistent state |
| `whisper_model_path` | Path to Whisper model for voice transcription |
| `tts_endpoint` | XTTS API URL for voice output |

## Bot Capabilities

The chatbot can:
- `send_message` - send messages to chats
- `send_photo` - generate and send AI images (Gemini)
- `send_voice` - send voice messages via TTS (XTTS)
- `add_reaction` - react to messages with emoji
- `read_messages` - search message history
- `get_user_info` - look up user details
- `get_members` - list tracked group members
- `delete_message` - remove messages (admin)
- `mute_user` - temporarily mute users (admin)
- `kick_user` - kick users from group (admin)
- `ban_user` - permanently ban users (admin)

Voice input is automatically transcribed via Whisper when configured.

## Security

- Claude Code runs with `--tools ""` (all tools disabled) to prevent RCE
- Message content is XML-escaped to prevent prompt injection
- User IDs from Telegram are unforgeable; only message content is user-controlled
- Path traversal protection on file imports

## License

MIT
