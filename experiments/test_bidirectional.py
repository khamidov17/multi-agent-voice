#!/usr/bin/env python3
"""
Test bidirectional JSON stdio - can we inject messages mid-conversation?
This would allow multi-turn tool use without spawning multiple processes.
"""

import subprocess
import json
import threading
import time

def main():
    print("Testing bidirectional JSON stdio")
    print("=" * 60)

    proc = subprocess.Popen(
        [
            "claude",
            "--print",
            "--output-format", "stream-json",
            "--input-format", "stream-json",  # Enable stdin input
            "--verbose",
            "--tools", "",  # No built-in tools
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        stdin=subprocess.PIPE,
        text=True,
    )

    # Read stdout in a thread
    def read_output():
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
                msg_type = msg.get("type", "unknown")

                if msg_type == "assistant":
                    content = msg.get("message", {}).get("content", [])
                    for block in content:
                        if block.get("type") == "text":
                            print(f"[assistant]: {block.get('text')[:200]}")
                elif msg_type == "result":
                    print(f"[result] cost=${msg.get('total_cost_usd', 0):.4f}")
                elif msg_type == "system":
                    print(f"[system] ready")
                else:
                    print(f"[{msg_type}]")
            except json.JSONDecodeError:
                print(f"[raw]: {line[:100]}")

    reader = threading.Thread(target=read_output, daemon=True)
    reader.start()

    # Send first message
    time.sleep(0.5)  # Wait for init

    first_msg = {
        "type": "user",
        "message": {
            "role": "user",
            "content": "Say hello briefly"
        }
    }
    print(f"\n[sending]: user message")
    proc.stdin.write(json.dumps(first_msg) + "\n")
    proc.stdin.flush()

    # Wait for response
    time.sleep(5)

    # Send second message
    second_msg = {
        "type": "user",
        "message": {
            "role": "user",
            "content": "Now say goodbye"
        }
    }
    print(f"\n[sending]: second user message")
    proc.stdin.write(json.dumps(second_msg) + "\n")
    proc.stdin.flush()

    # Wait for response
    time.sleep(5)

    # Close stdin to signal done
    proc.stdin.close()
    proc.wait()

    stderr = proc.stderr.read()
    if stderr:
        print(f"\n[stderr]: {stderr[:500]}")

if __name__ == "__main__":
    main()
