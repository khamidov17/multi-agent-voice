#!/usr/bin/env python3
"""Lyria music generation via Google BidiGenerateMusicContent WebSocket API.

Writes OGG Opus audio to stdout (binary).
Logs debug info to stderr.

Usage: python3 scripts/lyria_music.py <api_key> <prompt>

Requires: pip3 install websockets
"""

import asyncio
import base64
import json
import os
import subprocess
import sys
import tempfile


async def generate(api_key: str, prompt: str) -> bytes:
    try:
        import websockets
    except ImportError:
        print("ERROR: websockets not installed. Run: pip3 install websockets", file=sys.stderr)
        sys.exit(1)

    url = (
        "wss://generativelanguage.googleapis.com/ws/"
        "google.ai.generativelanguage.v1alpha.GenerativeService"
        f".BidiGenerateMusicContent?key={api_key}"
    )

    target_bytes = 5_760_000  # ~30 seconds of 48kHz stereo 16-bit PCM

    async with websockets.connect(url, ping_interval=None, open_timeout=20) as ws:
        # Step 1: Setup
        await ws.send(json.dumps({"setup": {"model": "models/lyria-realtime-exp"}}))
        print("Sent setup message", file=sys.stderr)

        # Step 2: Wait for setupComplete
        setup_done = False
        for _ in range(20):
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=15)
            except asyncio.TimeoutError:
                print("ERROR: Timed out waiting for setupComplete", file=sys.stderr)
                sys.exit(1)

            if isinstance(msg, bytes):
                print(f"Binary setup msg ({len(msg)} bytes) — treating as setupComplete", file=sys.stderr)
                setup_done = True
                break
            elif isinstance(msg, str):
                print(f"Setup text msg: {msg[:300]}", file=sys.stderr)
                try:
                    data = json.loads(msg)
                except json.JSONDecodeError:
                    continue
                if "setupComplete" in data or "setup_complete" in data:
                    print("Got setupComplete", file=sys.stderr)
                    setup_done = True
                    break

        if not setup_done:
            print("ERROR: Never received setupComplete", file=sys.stderr)
            sys.exit(1)

        # Step 3: Send music config (prompts + generation parameters)
        config_msg = {
            "musicGenerationConfig": {
                "weightedPrompts": [{"text": prompt, "weight": 1.0}],
                "bpm": 120,
                "guidance": 4.0,
                "density": 0.6,
                "brightness": 0.5,
            }
        }
        await ws.send(json.dumps(config_msg))
        print(f"Sent musicGenerationConfig: {prompt!r}", file=sys.stderr)

        # Step 4: Start playback
        await ws.send(json.dumps({"playbackControl": {"play": {}}}))
        print("Sent play command", file=sys.stderr)

        # Step 5: Collect audio chunks
        pcm_chunks: list[bytes] = []
        total_bytes = 0

        while total_bytes < target_bytes:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=10)
            except asyncio.TimeoutError:
                print(f"Read timeout, collected {total_bytes} bytes", file=sys.stderr)
                break

            if isinstance(msg, bytes):
                pcm_chunks.append(msg)
                total_bytes += len(msg)
                print(f"Binary chunk: {len(msg)} bytes, total: {total_bytes}", file=sys.stderr)
                continue

            # Text JSON message
            try:
                data = json.loads(msg)
            except json.JSONDecodeError:
                print(f"Non-JSON text: {msg[:100]}", file=sys.stderr)
                continue

            # Extract audio chunks (try both camelCase and snake_case)
            chunks = (
                data.get("serverContent", {}).get("audioChunks")
                or data.get("server_content", {}).get("audio_chunks")
                or []
            )
            for chunk in chunks:
                b64 = chunk.get("data", "")
                if b64:
                    decoded = base64.b64decode(b64)
                    pcm_chunks.append(decoded)
                    total_bytes += len(decoded)

            if chunks:
                print(f"JSON audio chunks: {len(chunks)}, total: {total_bytes}", file=sys.stderr)
            elif "serverContent" in data or "server_content" in data:
                print(f"Server msg (no audio): {msg[:200]}", file=sys.stderr)

        print(f"Collected {total_bytes} bytes of PCM", file=sys.stderr)

    if not pcm_chunks:
        print("ERROR: No audio data received from Lyria", file=sys.stderr)
        sys.exit(1)

    pcm_data = b"".join(pcm_chunks)

    # Step 6: Convert raw PCM (16-bit LE, 48kHz, stereo) to OGG Opus via ffmpeg
    with tempfile.NamedTemporaryFile(suffix=".pcm", delete=False) as f:
        f.write(pcm_data)
        pcm_path = f.name

    try:
        result = subprocess.run(
            [
                "ffmpeg", "-y",
                "-f", "s16le", "-ar", "48000", "-ac", "2",
                "-i", pcm_path,
                "-c:a", "libopus", "-b:a", "128k",
                "-f", "ogg", "pipe:1",
            ],
            capture_output=True,
        )
        if result.returncode != 0:
            print(f"ERROR: ffmpeg failed: {result.stderr.decode()}", file=sys.stderr)
            sys.exit(1)
        return result.stdout
    finally:
        os.unlink(pcm_path)


def main() -> None:
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <api_key> <prompt>", file=sys.stderr)
        sys.exit(1)

    api_key = sys.argv[1]
    prompt = sys.argv[2]

    ogg_data = asyncio.run(generate(api_key, prompt))
    sys.stdout.buffer.write(ogg_data)
    sys.stdout.buffer.flush()
    print(f"Generated {len(ogg_data)} bytes of OGG audio", file=sys.stderr)


if __name__ == "__main__":
    main()
