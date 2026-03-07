#!/bin/bash
# Setup script for voice transcription tests.
#
# Requirements:
# - ffmpeg: for audio conversion
# - espeak: for generating test audio (or use your own recordings)
#
# Downloads Whisper model and creates test audio files.

set -e

TEST_DIR="data/test"
AUDIO_DIR="$TEST_DIR/audio"
MODEL_PATH="$TEST_DIR/ggml-base.en.bin"

mkdir -p "$AUDIO_DIR"

# Download Whisper model if not present
if [ ! -f "$MODEL_PATH" ]; then
    echo "Downloading Whisper base.en model..."
    wget -O "$MODEL_PATH" \
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
    echo "Model downloaded to $MODEL_PATH"
else
    echo "Model already exists at $MODEL_PATH"
fi

# Generate test audio using espeak (if available)
if command -v espeak &> /dev/null; then
    echo "Generating test audio files..."

    # hello.ogg - simple greeting
    espeak "hello" --stdout | ffmpeg -y -i pipe: -c:a libopus "$AUDIO_DIR/hello.ogg"
    echo "Created hello.ogg"

    # test_phrase.ogg - longer phrase for E2E test
    espeak "this is a test of voice transcription" --stdout | ffmpeg -y -i pipe: -c:a libopus "$AUDIO_DIR/test_phrase.ogg"
    echo "Created test_phrase.ogg"

    echo "Test audio files created in $AUDIO_DIR"
else
    echo "espeak not found. Please install it or create test audio manually:"
    echo "  - $AUDIO_DIR/hello.ogg: say 'hello'"
    echo "  - $AUDIO_DIR/test_phrase.ogg: say any phrase"
    echo ""
    echo "You can record using Telegram and export the voice messages."
fi

echo ""
echo "Setup complete! Run tests with:"
echo "  cargo test --features integ_test --test voice_transcription"
