#!/usr/bin/env python3
"""
Test Claude CLI with tool calls to see the full message format.
"""

import subprocess
import json

def main():
    # This prompt should trigger tool use (Read tool to check a file)
    prompt = "Read the first 5 lines of Cargo.toml and tell me the package name. Be brief."

    print(f"Prompt: {prompt!r}")
    print("=" * 60)

    proc = subprocess.Popen(
        ["claude", "--print", "--output-format", "stream-json", "--verbose", "--", prompt],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    messages = []

    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue

        try:
            msg = json.loads(line)
            messages.append(msg)

            msg_type = msg.get("type", "unknown")
            subtype = msg.get("subtype", "")

            print(f"\n--- [{msg_type}] {subtype} ---")

            # Show relevant parts based on message type
            if msg_type == "system":
                print(f"  session_id: {msg.get('session_id')}")
                print(f"  model: {msg.get('model')}")

            elif msg_type == "assistant":
                content = msg.get("message", {}).get("content", [])
                for block in content:
                    block_type = block.get("type")
                    if block_type == "text":
                        print(f"  [text]: {block.get('text', '')[:200]}")
                    elif block_type == "tool_use":
                        print(f"  [tool_use]: {block.get('name')} (id: {block.get('id')})")
                        print(f"    input: {json.dumps(block.get('input', {}))[:150]}")

            elif msg_type == "user":
                content = msg.get("message", {}).get("content", [])
                for block in content:
                    block_type = block.get("type")
                    if block_type == "tool_result":
                        tool_id = block.get("tool_use_id", "")
                        result = block.get("content", "")
                        # Truncate long results
                        if len(result) > 200:
                            result = result[:200] + "..."
                        print(f"  [tool_result] id={tool_id[:20]}...")
                        print(f"    content: {result[:150]}")

            elif msg_type == "result":
                print(f"  success: {msg.get('subtype') == 'success'}")
                print(f"  cost: ${msg.get('total_cost_usd', 0):.4f}")
                print(f"  turns: {msg.get('num_turns')}")
                print(f"  result: {msg.get('result', '')[:200]}")

        except json.JSONDecodeError as e:
            print(f"Parse error: {e}")

    proc.wait()

    stderr = proc.stderr.read()
    if stderr:
        print(f"\n[stderr]\n{stderr}")

    print("\n" + "=" * 60)
    print(f"Exit code: {proc.returncode}")
    print(f"Total messages: {len(messages)}")

if __name__ == "__main__":
    main()
