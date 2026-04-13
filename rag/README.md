# RAG Knowledge Base

Drop your resources here. Nova runs `python rag/index.py` and all agents get the knowledge.

## Folders

```
rag/
├── knowledge/
│   ├── papers/     ← Drop PDF files here (research papers)
│   ├── repos/      ← Clone GitHub repos here
│   ├── links/      ← Drop .txt files with URLs (one per line)
│   └── docs/       ← Drop any text files, markdown, code snippets
├── index/          ← Auto-generated embeddings (don't touch)
├── sources.yaml    ← List of all sources (auto-updated)
├── index.py        ← Run this to build/rebuild the index
├── query.py        ← Query the knowledge base
└── README.md       ← This file
```

## How to Add Knowledge

1. **PDF papers:** Drop into `knowledge/papers/`
2. **GitHub repos:** Clone into `knowledge/repos/` (e.g. `cd knowledge/repos && git clone https://github.com/...`)
3. **URLs:** Create a .txt file in `knowledge/links/` with one URL per line
4. **Any docs:** Drop text/markdown/code files into `knowledge/docs/`

## How to Build Index

```bash
cd rag && python index.py
```

This reads all files, chunks them, generates embeddings, and saves the index.
All agents can then query it.

## How Agents Query

Sentinel/Nova/Atlas call:
```bash
cd rag && python query.py "how does ECAPA-TDNN extract speaker embeddings?"
```

Returns the top-5 most relevant chunks with source citations.
