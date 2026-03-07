#!/usr/bin/env python3
"""
XTTS TTS Server for Claudir.

A simple HTTP server that provides text-to-speech using Coqui XTTS.
Compatible with the Fish Speech API format for easy migration.

Usage:
    python scripts/xtts_server.py [--port 8880] [--voices-dir data/voices]

The server loads XTTS v2 model and serves TTS requests.
Voice cloning is supported by placing reference .wav files in the voices directory.
"""

import argparse
import io
import os
import sys
from pathlib import Path

# Fix for PyTorch 2.6+ strict weights_only default
import torch
_original_load = torch.load
def _patched_load(*args, **kwargs):
    if 'weights_only' not in kwargs:
        kwargs['weights_only'] = False
    return _original_load(*args, **kwargs)
torch.load = _patched_load

from flask import Flask, jsonify, request, send_file
from TTS.api import TTS

app = Flask(__name__)

# Global TTS instance
tts = None
voices_dir = None
device = None


def load_model():
    """Load XTTS model."""
    global tts, device

    # Use GPU if available
    device = "cuda" if torch.cuda.is_available() else "cpu"
    print(f"Loading XTTS v2 on {device}...")

    # Load XTTS v2 - multilingual model with voice cloning
    tts = TTS("tts_models/multilingual/multi-dataset/xtts_v2").to(device)
    print("XTTS model loaded successfully")


@app.route("/v1/references/list", methods=["GET"])
def list_references():
    """List available voice references."""
    if not voices_dir or not voices_dir.exists():
        return jsonify({"success": True, "reference_ids": ["default"]})

    # Find all .wav files in voices directory
    voices = []
    for f in voices_dir.glob("*.wav"):
        voices.append(f.stem)

    if not voices:
        voices = ["default"]

    return jsonify({"success": True, "reference_ids": sorted(voices)})


@app.route("/v1/tts", methods=["POST"])
def synthesize():
    """Synthesize speech from text."""
    global tts, voices_dir

    data = request.json
    if not data:
        return jsonify({"error": "No JSON data"}), 400

    text = data.get("text", "")
    if not text:
        return jsonify({"error": "No text provided"}), 400

    reference_id = data.get("reference_id", "default")
    language = data.get("language", "en")

    print(f"TTS: \"{text[:50]}...\" (voice={reference_id}, lang={language})")

    try:
        # Find reference audio for voice cloning
        speaker_wav = None
        if voices_dir and reference_id != "default":
            ref_path = voices_dir / f"{reference_id}.wav"
            if ref_path.exists():
                speaker_wav = str(ref_path)
                print(f"Using voice reference: {ref_path}")

        # Generate speech
        wav_buffer = io.BytesIO()

        if speaker_wav:
            # Voice cloning mode
            tts.tts_to_file(
                text=text,
                file_path=wav_buffer,
                speaker_wav=speaker_wav,
                language=language,
            )
        else:
            # Default voice mode (use built-in speaker)
            tts.tts_to_file(
                text=text,
                file_path=wav_buffer,
                speaker="Ana Florence",  # Default XTTS speaker
                language=language,
            )

        wav_buffer.seek(0)
        return send_file(
            wav_buffer,
            mimetype="audio/wav",
            as_attachment=False,
        )

    except Exception as e:
        print(f"TTS error: {e}", file=sys.stderr)
        return jsonify({"error": str(e)}), 500


@app.route("/health", methods=["GET"])
def health():
    """Health check endpoint."""
    return jsonify({"status": "ok", "model": "xtts_v2", "device": device})


def main():
    global voices_dir

    parser = argparse.ArgumentParser(description="XTTS TTS Server")
    parser.add_argument("--port", type=int, default=8880, help="Port to listen on")
    parser.add_argument("--host", default="127.0.0.1", help="Host to bind to")
    parser.add_argument("--voices-dir", type=Path, default=None, help="Directory with voice reference WAV files")
    args = parser.parse_args()

    voices_dir = args.voices_dir
    if voices_dir:
        voices_dir = Path(voices_dir).resolve()
        if not voices_dir.exists():
            print(f"Creating voices directory: {voices_dir}")
            voices_dir.mkdir(parents=True, exist_ok=True)

    # Load model before starting server
    load_model()

    print(f"Starting XTTS server on {args.host}:{args.port}")
    if voices_dir:
        print(f"Voice references directory: {voices_dir}")

    # Run Flask server
    app.run(host=args.host, port=args.port, threaded=True)


if __name__ == "__main__":
    main()
