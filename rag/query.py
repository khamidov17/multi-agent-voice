#!/usr/bin/env python3
"""Query the RAG knowledge base.

Returns the top-K most relevant chunks with source citations.
Designed to be called by any agent (Nova, Sentinel, Atlas).

Model loading is cached in a module-level global so that when this
script is imported (rather than run as a subprocess) the model is only
loaded once.  When called as a one-shot subprocess the ~2-3s load time
is unavoidable; use rag/server.py for a persistent HTTP query server
that eliminates this overhead entirely.

Usage:
  python rag/query.py "how does ECAPA-TDNN work?"
  python rag/query.py "VoicePrivacy Challenge evaluation metrics" --top-k 10
  python rag/query.py "speaker embedding extraction" --json

Dependencies:
  pip install sentence-transformers chromadb
"""

import argparse
import json
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
INDEX_DIR = SCRIPT_DIR / "index"

# Module-level model cache — avoids reloading when imported as a library.
_MODEL = None
_MODEL_NAME = "BAAI/bge-base-en-v1.5"


def _get_model():
    """Return cached embedding model, loading it on first call."""
    global _MODEL
    if _MODEL is None:
        from sentence_transformers import SentenceTransformer
        _MODEL = SentenceTransformer(_MODEL_NAME)
    return _MODEL


def query(question: str, top_k: int = 5, as_json: bool = False) -> str:
    """Query the knowledge base and return relevant chunks.

    The question is prefixed with "query: " as required by BGE models for
    asymmetric retrieval (passages are indexed with "passage: " prefix).
    """
    import chromadb

    client = chromadb.PersistentClient(path=str(INDEX_DIR))
    try:
        collection = client.get_collection("knowledge")
    except Exception:
        return "ERROR: No index found. Run: python rag/index.py"

    model = _get_model()
    # BGE asymmetric retrieval: queries use "query: " prefix
    query_text = "query: " + question
    query_embedding = model.encode([query_text], normalize_embeddings=True)[0].tolist()

    results = collection.query(
        query_embeddings=[query_embedding],
        n_results=top_k,
    )

    if not results["documents"] or not results["documents"][0]:
        return "No relevant results found."

    if as_json:
        entries = []
        for doc, meta, dist in zip(
            results["documents"][0],
            results["metadatas"][0],
            results["distances"][0],
        ):
            entries.append({
                "text": doc,
                "source": meta.get("source", "unknown"),
                "relevance": round(1 - dist, 3),
            })
        return json.dumps(entries, indent=2)

    # Human-readable output
    lines = [f"Query: {question}", f"Results: {len(results['documents'][0])} matches", ""]
    for i, (doc, meta, dist) in enumerate(zip(
        results["documents"][0],
        results["metadatas"][0],
        results["distances"][0],
    )):
        relevance = round(1 - dist, 3)
        source = meta.get("source", "unknown")
        lines.append(f"--- [{i+1}] relevance={relevance} source={source} ---")
        lines.append(doc[:500])
        lines.append("")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Query RAG knowledge base")
    parser.add_argument("question", help="Your question")
    parser.add_argument("--top-k", type=int, default=5, help="Number of results (default: 5)")
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    args = parser.parse_args()

    result = query(args.question, top_k=args.top_k, as_json=args.json)
    print(result)


if __name__ == "__main__":
    main()
