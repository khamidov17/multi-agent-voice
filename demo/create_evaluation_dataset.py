#!/usr/bin/env python3
"""Create complete evaluation dataset with metadata for EER/WER/PMOS testing.

Generates:
1. speaker_mapping.tsv - file_id + speaker_id mappings for EER
2. transcriptions.tsv - ground truth transcriptions for WER
3. evaluation_pairs.tsv - original/anonymized file pairs
"""
import os
import whisper
import csv
from pathlib import Path

# Paths
DEMO_DIR = Path(__file__).parent
OUTPUT_DIR = DEMO_DIR / "output"
EVAL_DIR = DEMO_DIR / "evaluation"
EVAL_DIR.mkdir(exist_ok=True)

# File naming patterns from demo output
ORIGINAL_FILES = [
    "1_single_clean_original.wav",
    "2_single_noisy_noisy.wav",
    "3_multi_clean_original.wav",
    "4_multi_noisy_noisy.wav",
]

ANONYMIZED_VARIANTS = {
    "1_single_clean": ["ohnn", "selection"],
    "2_single_noisy": ["denoised", "ohnn", "selection"],
    "3_multi_clean": ["ohnn", "selection", "tsa_ohnn", "tsa_selection"],
    "4_multi_noisy": ["denoised", "ohnn", "selection", "tsa_ohnn", "tsa_selection"],
}

# Speaker mappings (extracted from filenames in sample_audio)
# Based on: sample_01_spk1089.wav -> dialogue_01_spk1089_spk1188.wav
SPEAKER_MAPPINGS = {
    "1_single_clean": ["spk1089"],  # single speaker
    "2_single_noisy": ["spk1089"],  # single speaker with noise
    "3_multi_clean": ["spk1089", "spk1188"],  # dialogue 01
    "4_multi_noisy": ["spk1089", "spk1188"],  # dialogue 01 with noise
}


def transcribe_audio(wav_path):
    """Transcribe audio using Whisper."""
    print(f"Transcribing {wav_path.name}...")
    model = whisper.load_model("base")
    result = model.transcribe(str(wav_path))
    return result["text"].strip()


def create_speaker_mapping():
    """Create speaker_mapping.tsv for EER evaluation."""
    rows = []

    for prefix, speakers in SPEAKER_MAPPINGS.items():
        # Original file
        original = f"{prefix}_original.wav"
        for spk in speakers:
            rows.append([original, spk])

        # All anonymized variants have same speakers
        if prefix in ANONYMIZED_VARIANTS:
            for variant in ANONYMIZED_VARIANTS[prefix]:
                anon_file = f"{prefix}_{variant}.wav"
                for spk in speakers:
                    rows.append([anon_file, spk])

    tsv_path = EVAL_DIR / "speaker_mapping.tsv"
    with open(tsv_path, "w", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["file_id", "speaker_id"])
        writer.writerows(rows)

    print(f"✓ Created {tsv_path} ({len(rows)} entries)")
    return tsv_path


def create_transcriptions():
    """Create transcriptions.tsv for WER evaluation."""
    rows = []

    # Transcribe original files only (variants share same content)
    for orig in ORIGINAL_FILES:
        wav_path = OUTPUT_DIR / orig
        if not wav_path.exists():
            print(f"⚠ Missing: {orig}")
            continue

        text = transcribe_audio(wav_path)
        rows.append([orig, text])

    tsv_path = EVAL_DIR / "transcriptions.tsv"
    with open(tsv_path, "w", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["file_id", "transcription"])
        writer.writerows(rows)

    print(f"✓ Created {tsv_path} ({len(rows)} transcriptions)")
    return tsv_path


def create_evaluation_pairs():
    """Create evaluation_pairs.tsv mapping original -> anonymized."""
    rows = []

    for prefix, variants in ANONYMIZED_VARIANTS.items():
        original = f"{prefix}_original.wav"
        for variant in variants:
            anon_file = f"{prefix}_{variant}.wav"
            rows.append([original, anon_file, variant])

    tsv_path = EVAL_DIR / "evaluation_pairs.tsv"
    with open(tsv_path, "w", newline="") as f:
        writer = csv.writer(f, delimiter="\t")
        writer.writerow(["original_file", "anonymized_file", "method"])
        writer.writerows(rows)

    print(f"✓ Created {tsv_path} ({len(rows)} pairs)")
    return tsv_path


if __name__ == "__main__":
    print("Creating evaluation dataset...\n")

    # 1. Speaker mapping for EER
    speaker_tsv = create_speaker_mapping()

    # 2. Ground truth transcriptions for WER
    transcriptions_tsv = create_transcriptions()

    # 3. Original/anonymized pairs
    pairs_tsv = create_evaluation_pairs()

    print(f"\n✅ Evaluation dataset complete at {EVAL_DIR}/")
    print(f"   - {speaker_tsv.name}")
    print(f"   - {transcriptions_tsv.name}")
    print(f"   - {pairs_tsv.name}")
