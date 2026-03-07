#!/usr/bin/env python3
"""
Minimal experiment: spawn Claude Code CLI and capture JSON output.
Tests whether CLI subprocess uses Max subscription (no API key needed).
"""

import subprocess
import json
import sys

def main():
    prompt = "What is 2 + 2? Reply with just the number."

    print(f"Spawning claude CLI with prompt: {prompt!r}")
    print("=" * 60)

    # Spawn claude with JSON output
    # --print (-p): non-interactive mode
    # --output-format stream-json: NDJSON output
    # --verbose: required for stream-json with --print
    proc = subprocess.Popen(
        ["claude", "--print", "--output-format", "stream-json", "--verbose", "--", prompt],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    messages = []

    # Read NDJSON lines from stdout
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue

        try:
            msg = json.loads(line)
            messages.append(msg)

            # Pretty print each message
            msg_type = msg.get("type", "unknown")
            print(f"\n[{msg_type}]")
            print(json.dumps(msg, indent=2)[:500])  # Truncate long messages

        except json.JSONDecodeError as e:
            print(f"Failed to parse: {line[:100]}")
            print(f"Error: {e}")

    # Wait for process to finish
    proc.wait()

    # Print stderr if any
    stderr = proc.stderr.read()
    if stderr:
        print(f"\n[stderr]\n{stderr}")

    print("\n" + "=" * 60)
    print(f"Exit code: {proc.returncode}")
    print(f"Total messages: {len(messages)}")

    # Extract the actual response text
    for msg in messages:
        if msg.get("type") == "assistant":
            content = msg.get("message", {}).get("content", [])
            for block in content:
                if block.get("type") == "text":
                    print(f"\nClaude's response: {block.get('text')}")

        if msg.get("type") == "result":
            print(f"\nCost: ${msg.get('total_cost_usd', 'unknown')}")
            print(f"Turns: {msg.get('num_turns', 'unknown')}")

if __name__ == "__main__":
    main()
