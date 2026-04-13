# Voice Anonymization Skill

---

## The Development Loop

```
Owner shares goal/paper
  → CEO breaks into tasks with success criteria
  → CEO assigns to CTO
  → CTO shares PLAN (not code) in the group
  → CEO reviews plan: algorithm, structure, metrics
  → CEO approves (or sends back for revision)
  → CTO implements following the approved plan
  → CTO sends test audio to group via send_file
  → CEO evaluates: listens to audio + checks metrics
  → Debugger reviews code quality and security
  → CEO gives PASS/FAIL verdict
  → If FAIL: CEO assigns specific fix → CTO implements → loop
  → If PASS: next task
```

**Key rule: PLAN BEFORE CODE.** CTO never writes code without CEO approval.

---

## Code Quality Standards

### Project Structure (mandatory)
```
project/
├── README.md
├── requirements.txt
├── src/                    # All source code
│   ├── __init__.py
│   ├── pipeline.py
│   ├── feature_extraction.py
│   ├── speaker_embedding.py
│   ├── vocoder.py
│   └── utils.py
├── eval/                   # Evaluation scripts
├── tests/                  # Test suite
├── configs/                # Configuration
└── output/                 # Generated files (gitignored)
```

**NEVER:** flat file dumps, v2/v3 copies, random GitHub clones, toy algorithms

### Algorithm Standards
- Use SOTA from VoicePrivacy Challenge and INTERSPEECH publications
- Proper speaker embedding extraction (ECAPA-TDNN, not MFCC hacks)
- HiFi-GAN or equivalent neural vocoder for resynthesis
- Validated evaluation metrics (MOS, EER, Speaker Similarity, WER)

---

## Pipeline: OHNN (One-Hot Neural Network)

```
1. Feature Extraction
   Input → resample 16kHz mono → mel spectrogram (80 mels, hop 256) → F0 pitch (CREPE/DIO)
   → speaker embedding (ECAPA-TDNN, 192-dim)

2. Speaker Anonymization
   Source embedding → cosine distance against pool → select target (distance > 0.7)
   → deterministic selection (seeded RNG for reproducibility)

3. Speech Resynthesis
   HiFi-GAN: (mel + source_F0 + target_embedding) → waveform

4. Post-processing
   EBU R128 loudness normalization → silence trimming → validate 16kHz mono
   Peak < -1 dBFS
```

**Invariants:**
- F0 from SOURCE speaker (preserves prosody)
- Duration preserved (no time-stretching)
- Deterministic output (seeded RNG)
- 16kHz mono WAV output

---

## Evaluation

### Metrics (VoicePrivacy Challenge)
| Metric | Tool | Target | Fail |
|--------|------|--------|------|
| MOS | UTMOS | ≥ 3.5 | < 3.0 |
| EER | ECAPA-TDNN | ≤ 10% | > 25% |
| Speaker Similarity | resemblyzer | ≤ 0.30 | > 0.50 |
| WER | Whisper + jiwer | ≤ 15% | > 30% |

### What counts as valid testing:
- Audio files sent to Telegram group via send_file for human review
- Metrics computed on 10+ diverse utterances
- Original vs anonymized comparison
- NOT: "tests pass" without audio evidence

### When owner shares a PDF/paper:
- CEO extracts key techniques and evaluation methods
- If paper introduces better metrics → update thresholds
- If paper proposes better algorithm → CEO assigns CTO to evaluate

---

## Decision Tree (when metrics fail)

```
MOS < 3.5 → vocoder quality, F0 smoothing, reduce anonymization strength
EER > 10% → increase embedding distance threshold, check extractor
Speaker Sim > 0.30 → pool diversity, gender-crossing, pool size
WER > 15% → F0 conditioning, output normalization, check clipping
Stagnation → switch algorithm variant entirely
```

---

## Communication Rules

- ONE message per response
- No fluffy narration ("let me check...", "standing by...")
- Direct and concise
- Report results with evidence, not promises
- CTO: plan first, code second
- CEO: evaluate with metrics, not enthusiasm
