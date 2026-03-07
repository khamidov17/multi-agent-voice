#!/usr/bin/env python3
"""
Test Claude CLI with no tools - pure LLM mode.
This is closer to how we'd use it for the chatbot.
"""

import subprocess
import json

def main():
    # Simulate chatbot system prompt + user message
    system_prompt = """You are a helpful assistant. When you want to respond, output JSON like:
{"action": "send_message", "text": "your response here"}
When done, output:
{"action": "done"}"""

    user_message = "Hey, what's up?"

    # Combine into a single prompt
    prompt = f"""<system>
{system_prompt}
</system>

<user>
{user_message}
</user>"""

    print(f"Testing with no tools enabled")
    print("=" * 60)

    proc = subprocess.Popen(
        [
            "claude",
            "--print",
            "--output-format", "stream-json",
            "--verbose",
            "--tools", "",  # Disable all tools
            "--",
            prompt
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue

        try:
            msg = json.loads(line)
            msg_type = msg.get("type", "unknown")

            if msg_type == "system":
                print(f"[system] model={msg.get('model')}, tools={msg.get('tools')}")

            elif msg_type == "assistant":
                content = msg.get("message", {}).get("content", [])
                for block in content:
                    if block.get("type") == "text":
                        print(f"\n[assistant text]:\n{block.get('text')}")

            elif msg_type == "result":
                print(f"\n[result] cost=${msg.get('total_cost_usd', 0):.4f}")

        except json.JSONDecodeError:
            pass

    proc.wait()
    stderr = proc.stderr.read()
    if stderr:
        print(f"\n[stderr] {stderr}")

if __name__ == "__main__":
    main()
