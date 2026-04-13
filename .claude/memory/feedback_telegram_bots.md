---
name: telegram_bot_visibility
description: Telegram bots cannot see other bots' messages in groups unless they are admins or privacy mode is disabled
type: feedback
---

Telegram bots in groups can ONLY see: messages mentioning @bot, replies to bot's messages, /commands, and service messages. Bots CANNOT see other bots' messages AT ALL.

**Why:** Telegram's default privacy mode for bots restricts what they receive in groups.

**How to apply:** When setting up multi-bot systems:
1. Make all bots group admins (admins see all messages), OR
2. Disable privacy mode via BotFather (/setprivacy → Disable), OR
3. Use shared SQLite bot-to-bot messaging (claudir architecture pattern)

This was the root cause of bots not responding to each other — they literally never received the messages.
