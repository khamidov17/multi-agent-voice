#!/usr/bin/env python3
"""
Fetch Telegram group history using Telethon.

Usage:
    pip install telethon
    python fetch_history.py <chat_id> <output.json>

Example:
    python fetch_history.py -1001506871265 history.json

First run will ask for phone number and auth code.
"""

import asyncio
import json
import sys
import os
from datetime import datetime

from telethon import TelegramClient
from telethon.tl.types import User, Channel, Chat

# Get these from https://my.telegram.org/apps
API_ID = os.environ.get('TELEGRAM_API_ID')
API_HASH = os.environ.get('TELEGRAM_API_HASH')

async def main():
    if len(sys.argv) != 3:
        print("Usage: python fetch_history.py <chat_id> <output.json>")
        print("Example: python fetch_history.py -1001506871265 history.json")
        print()
        print("Set TELEGRAM_API_ID and TELEGRAM_API_HASH environment variables")
        print("Get them from https://my.telegram.org/apps")
        sys.exit(1)

    if not API_ID or not API_HASH:
        print("ERROR: Set TELEGRAM_API_ID and TELEGRAM_API_HASH environment variables")
        print("Get them from https://my.telegram.org/apps")
        sys.exit(1)

    chat_id = int(sys.argv[1])
    output_path = sys.argv[2]

    print(f"Connecting to Telegram...")

    # Session file stores auth, so you only need to auth once
    client = TelegramClient('claudir_export', int(API_ID), API_HASH)
    await client.start()

    print(f"Fetching messages from chat {chat_id}...")
    print("This may take a while for large groups...")

    messages = []
    count = 0

    # Build user cache for resolving usernames
    user_cache = {}

    async for msg in client.iter_messages(chat_id, limit=None):
        count += 1
        if count % 500 == 0:
            print(f"  Fetched {count} messages...")

        # Skip non-text messages
        if not msg.text:
            continue

        # Get sender info
        sender = msg.sender
        if sender:
            if isinstance(sender, User):
                user_id = sender.id
                username = sender.username or sender.first_name or "unknown"
            elif isinstance(sender, (Channel, Chat)):
                user_id = sender.id
                username = sender.title or "channel"
            else:
                user_id = 0
                username = "unknown"
            user_cache[user_id] = username
        else:
            user_id = 0
            username = "unknown"

        # Build reply_to
        reply_to = None
        if msg.reply_to and msg.reply_to.reply_to_msg_id:
            reply_id = msg.reply_to.reply_to_msg_id
            # We'll fill in reply details later if we have them
            reply_to = {
                "message_id": reply_id,
                "username": "",  # Will be filled from cache
                "text": ""
            }

        timestamp = msg.date.strftime("%H:%M") if msg.date else "00:00"

        messages.append({
            "message_id": msg.id,
            "chat_id": chat_id,
            "user_id": user_id,
            "username": username,
            "timestamp": timestamp,
            "text": msg.text,
            "reply_to": reply_to,
            # Keep date for sorting
            "_date": msg.date.isoformat() if msg.date else None
        })

    print(f"Total messages fetched: {count}")
    print(f"Text messages: {len(messages)}")

    # Build message lookup for reply resolution
    msg_lookup = {m["message_id"]: m for m in messages}

    # Resolve reply_to details
    for msg in messages:
        if msg["reply_to"]:
            reply_id = msg["reply_to"]["message_id"]
            if reply_id in msg_lookup:
                original = msg_lookup[reply_id]
                msg["reply_to"]["username"] = original["username"]
                msg["reply_to"]["text"] = original["text"][:100]  # Truncate
            else:
                msg["reply_to"]["username"] = "unknown"

    # Sort by message_id (chronological)
    messages.sort(key=lambda m: m["message_id"])

    # Remove internal _date field
    for msg in messages:
        del msg["_date"]

    # Load existing messages if file exists
    existing = {}
    if os.path.exists(output_path):
        print(f"Loading existing messages from {output_path}...")
        with open(output_path, 'r') as f:
            data = json.load(f)
            for m in data.get("messages", []):
                existing[m["message_id"]] = m
        print(f"Existing messages: {len(existing)}")

    # Merge (new messages fill in gaps, don't overwrite)
    new_count = 0
    for msg in messages:
        if msg["message_id"] not in existing:
            existing[msg["message_id"]] = msg
            new_count += 1

    # Sort by message_id
    all_messages = sorted(existing.values(), key=lambda m: m["message_id"])

    print(f"New messages added: {new_count}")
    print(f"Total messages: {len(all_messages)}")

    # Save
    output = {"messages": all_messages}
    with open(output_path, 'w') as f:
        json.dump(output, f, indent=2, ensure_ascii=False)

    print(f"Saved to {output_path}")

    await client.disconnect()

if __name__ == "__main__":
    asyncio.run(main())
