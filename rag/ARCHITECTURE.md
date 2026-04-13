# RAG System Architecture

## Overview

A curated knowledge retrieval system for the agent team. The owner drops papers, repos, links, and docs into `knowledge/`. Nova runs `index.py` to build a vector index. All agents query it before making decisions.

## File Structure

```
rag/
├── ARCHITECTURE.md      ← This file
├── README.md            ← Quick-start guide
├── index.py             ← Build/rebuild the vector index (incremental by default)
├── query.py             ← One-shot CLI query tool
├── server.py            ← Persistent HTTP query server (localhost:7432)
├── log_experiment.py    ← Experiment logger (Sentinel writes, all agents read)
├── knowledge/           ← YOUR CURATED SOURCES (drop files here)
│   ├── papers/          ← PDF research papers
│   ├── repos/           ← Cloned GitHub repositories
│   ├── links/           ← .txt files with URLs (one per line)
│   └── docs/            ← Any text, markdown, code, notes
├── index/               ← Auto-generated (don't touch)
│   ├── chroma.sqlite3   ← ChromaDB vector storage
│   ├── .file_hashes.json ← SHA-256 per source file (for incremental)
│   └── .version.json    ← Last 3 build timestamps + stats
└── sources.yaml         ← Auto-generated list of indexed sources
```

This is NOT a general internet search. It's a **curated, private knowledge base** — only sources the owner explicitly adds are indexed.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  OWNER                                                          │
│  Drops resources into knowledge/{papers,repos,links,docs}       │
│  Tells Nova: "rebuild the knowledge base"                       │
└─────────────────┬───────────────────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  INDEX PIPELINE (rag/index.py)                                  │
│                                                                  │
│  1. Collect: scan knowledge/ subfolders                          │
│     - papers/*.pdf → PyMuPDF text extraction                     │
│     - repos/*/ → read .py, .rs, .md, .yaml, etc.                │
│     - links/*.txt → fetch URLs (GitHub raw / arxiv / trafilatura)│
│     - docs/* → read text/markdown files                          │
│                                                                  │
│  2. Hash check: SHA-256 per file, stored in index/.file_hashes   │
│     Only changed/new files are re-processed (incremental)        │
│                                                                  │
│  3. Chunk: sentence-boundary splitting (never mid-sentence)      │
│     Merge small pieces up to ~1000 chars, 200-char overlap seed  │
│     Each chunk gets a content_hash (MD5 of text)                 │
│                                                                  │
│  4. Deduplicate: drop chunks whose content_hash was already seen │
│     (across all sources — same text from two files = one entry)  │
│                                                                  │
│  5. Embed: BAAI/bge-base-en-v1.5 (768-dim)                      │
│     Passages encoded with "passage: " prefix (BGE asymmetric)    │
│     Queries encoded with "query: " prefix                        │
│                                                                  │
│  6. Store: ChromaDB upsert (content_hash as ID)                  │
│     Stale entries for changed sources are removed before upsert  │
│                                                                  │
│  7. Record: sources.yaml + index/.version.json (last 3 builds)  │
└─────────────────┬───────────────────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────────────────────┐
│  VECTOR INDEX (rag/index/)                                      │
│  ChromaDB persistent storage                                    │
│  Collection: "knowledge" (cosine similarity)                     │
│  Each entry: id (content MD5), text, embedding (768-dim), meta   │
│  Metadata files:                                                 │
│    .file_hashes.json  — SHA-256 per source file (incremental)   │
│    .version.json      — last 3 build timestamps + chunk counts   │
└─────────────────┬───────────────────────────────────────────────┘
                  │
              ┌───┴────────────┐
              ▼                ▼
┌─────────────────────┐  ┌──────────────────────────────────────────┐
│  QUERY (query.py)   │  │  QUERY SERVER (server.py)                │
│                     │  │                                          │
│  One-shot CLI tool. │  │  Persistent HTTP server on localhost:7432│
│  Model cached as    │  │  Loads model ONCE, serves all queries.   │
│  module global —    │  │  Endpoints:                              │
│  fast when imported │  │    GET /query?q=...&top_k=5&fmt=json     │
│  as a library,      │  │    GET /health                           │
│  ~2-3s if called    │  │    POST /shutdown                        │
│  as subprocess.     │  │  Use this for sub-100ms query latency.   │
└─────────────────────┘  └──────────────────────────────────────────┘
```

## Knowledge Folder Structure

```
rag/knowledge/
├── papers/          # PDF research papers
│   ├── voiceprivacy2024.pdf
│   ├── ecapa_tdnn.pdf
│   └── wavlm.pdf
│
├── repos/           # Cloned GitHub repositories
│   ├── MSA/         # git clone https://github.com/xiaoxiaomiao323/MSA
│   └── VPC2026/     # git clone https://github.com/Voice-Privacy-Challenge/...
│
├── links/           # Text files with URLs (one per line)
│   └── references.txt
│   # Contents:
│   # https://arxiv.org/abs/2203.12345
│   # https://github.com/speechbrain/speechbrain/blob/main/README.md
│   # # lines starting with # are comments
│
└── docs/            # Any text, markdown, code snippets
    ├── meeting_notes.md
    ├── evaluation_criteria.yaml
    └── custom_pipeline.py
```

## Commands

```bash
# Incremental update — only re-indexes changed/new files (default)
cd rag && python3 index.py

# Force full rebuild (ignores cached hashes)
cd rag && python3 index.py --full

# Show index statistics + version history
cd rag && python3 index.py --stats

# One-shot query (loads model each time, ~2-3s)
cd rag && python3 query.py "how does ECAPA-TDNN extract speaker embeddings?"
cd rag && python3 query.py "VoicePrivacy Challenge evaluation" --top-k 10
cd rag && python3 query.py "HiFi-GAN vocoder architecture" --json

# Persistent query server (loads model once, stays running)
python3 rag/server.py &
curl "http://localhost:7432/query?q=ECAPA-TDNN+architecture&top_k=5"
curl "http://localhost:7432/query?q=speaker+embeddings&fmt=json"
curl "http://localhost:7432/health"
```

## Experiment Logger

Separate from RAG but complementary. Tracks what methods were tried and their results, including which RAG queries were consulted before each decision.

```
┌─────────────────────────────────────────────────────────────────┐
│  EXPERIMENT LOG (rag/log_experiment.py)                          │
│                                                                  │
│  Storage: data/shared/experiments.jsonl (JSONL, append-only)     │
│  Written by: Sentinel (after every evaluation)                   │
│  Read by: All agents (before planning new work)                  │
│                                                                  │
│  Each entry:                                                     │
│  {                                                               │
│    "timestamp": "2026-04-12T11:23:21Z",                          │
│    "task": "OHNN pipeline v1",                                   │
│    "method": "ECAPA-TDNN + HiFi-GAN",                           │
│    "metrics": {"eer": 25.3, "wer": 12.1, "pmos": 3.8},          │
│    "verdict": "FAIL",                                            │
│    "notes": "EER below 30% threshold",                           │
│    "rag_sources": [                                              │
│      "ECAPA-TDNN architecture",                                  │
│      "VoicePrivacy Challenge evaluation metrics"                 │
│    ]                                                             │
│  }                                                               │
│                                                                  │
│  Commands:                                                       │
│    python3 rag/log_experiment.py --summary     # what was tried  │
│    python3 rag/log_experiment.py --view        # recent entries  │
│    python3 rag/log_experiment.py --search "X"  # find by keyword │
└─────────────────────────────────────────────────────────────────┘
```

## How Agents Use It

### Atlas (CEO)
- Before assigning a task: query RAG for relevant prior work
- Before approving a plan: check if the proposed method already failed
- Command: asks Nova to run `python3 rag/query.py "question"` (Atlas has no bash)

### Nova (CTO)
- Before coding: `python3 rag/log_experiment.py --summary` to check past attempts
- Before choosing an algorithm: `python3 rag/query.py "best approach for X"`
- After owner adds new papers: `cd rag && python3 index.py` to rebuild (incremental)
- For high-frequency querying: start `python3 rag/server.py` and use HTTP API

### Sentinel (Evaluator)
- After every evaluation: silently log to experiments.jsonl **including** the
  `--sources` list of RAG queries consulted before the decision
- Before evaluating: query RAG for expected metric ranges from literature
- Share experiment history with team when asked

## Dependencies

```
pip install sentence-transformers chromadb pymupdf trafilatura
```

- `sentence-transformers` — embedding model (BAAI/bge-base-en-v1.5, ~440MB)
- `chromadb` — vector database (local, no server needed)
- `pymupdf` (fitz) — PDF text extraction
- `trafilatura` — high-quality HTML boilerplate removal for web URLs

## Technical Details

### Embedding Model
- **Model:** `BAAI/bge-base-en-v1.5` (from sentence-transformers / HuggingFace)
- **Dimensions:** 768
- **Size:** ~440MB
- **Speed:** ~400 chunks/second on CPU (batch_size=32)
- **Quality:** State-of-the-art open-source retrieval model, top MTEB leaderboard
- **Asymmetric encoding:** Documents indexed with `"passage: "` prefix; queries
  use `"query: "` prefix (required for correct BGE retrieval quality)

### Chunking
- **Strategy:** Sentence-boundary splitting, then greedy merge up to CHUNK_SIZE
- **Boundaries tried:** `\n\n`, `\n`, `. `, `? `, `! `, `; ` (in priority order)
- **Chunk size:** ~1000 characters (soft limit — never splits mid-sentence)
- **Overlap:** Last 200 characters of each chunk seeded into the next
- **Long sentences:** If a single sentence exceeds chunk_size, word-boundary
  hard-split is used as a last resort

### URL Fetching
- **GitHub blob URLs:** Auto-converted to `raw.githubusercontent.com` for clean text
- **arXiv PDF URLs:** Converted to `/abs/` page for better extraction
- **All other URLs:** `trafilatura` library (removes ads, nav, boilerplate)
- **Fallback:** Regex HTML tag stripping if trafilatura unavailable

### Incremental Indexing
- **Hash file:** `rag/index/.file_hashes.json` — SHA-256 per source file/URL
- **On re-index:** Only files whose hash changed are re-processed
- **Stale cleanup:** Old ChromaDB entries for changed sources are deleted before
  new chunks are inserted (no accumulation of duplicates)
- **Force rebuild:** `python3 index.py --full` ignores all cached hashes

### Deduplication
- **ID scheme:** MD5 hash of chunk text content (not source path + index)
- **Cross-source:** Same text from a paper in `papers/` and `links/` → single entry
- **ChromaDB upsert:** content_hash used as the document ID, so re-indexing the
  same text is idempotent

### Storage
- **Backend:** ChromaDB (persistent, local SQLite + HNSW index)
- **Location:** `rag/index/` directory
- **Similarity:** Cosine distance
- **Capacity:** Handles 100K+ chunks easily on a single machine

### Versioning
- **Version file:** `rag/index/.version.json`
- **Contents:** timestamp, chunk_count, source_count, source list
- **Retention:** Last 3 builds kept (older entries pruned automatically)
- **View:** `python3 index.py --stats`

## Known Limitations

1. **Model cold start.** ~2-3s to load the 440MB BGE model per `query.py` subprocess
   call. Use `server.py` for latency-sensitive workflows.
2. **No query history.** We don't track what was queried or when (experiment logger
   tracks `rag_sources` for decisions, but not all ad-hoc queries).
3. **No access control.** All agents see all knowledge. No per-agent restrictions.
4. **URL content changes.** URLs are only re-fetched when `index.py` runs again.
   There is no background polling for updated web content.
