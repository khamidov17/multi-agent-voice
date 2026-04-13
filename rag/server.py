#!/usr/bin/env python3
"""Persistent RAG query server — eliminates per-query model load time.

Loads the embedding model ONCE on startup, then serves queries over HTTP
on localhost:7432.  Use this when you need sub-100ms query latency (vs
the ~2-3s cold start of running query.py as a subprocess every time).

Usage:
  # Start the server (background or separate terminal)
  python rag/server.py &

  # Query via HTTP (from any script or agent)
  curl -s "http://localhost:7432/query?q=ECAPA-TDNN+architecture&top_k=5"
  curl -s "http://localhost:7432/query?q=speaker+embeddings&top_k=5&fmt=json"

  # Health check
  curl -s "http://localhost:7432/health"

  # Stop
  curl -s -X POST "http://localhost:7432/shutdown"

The server listens only on 127.0.0.1 — never exposed to the network.

Dependencies:
  pip install sentence-transformers chromadb
"""

import json
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
INDEX_DIR = SCRIPT_DIR / "index"
HOST = "127.0.0.1"
PORT = 7432
MODEL_NAME = "BAAI/bge-base-en-v1.5"

# ---------------------------------------------------------------------------
# Global state loaded once at startup
# ---------------------------------------------------------------------------
_model = None
_collection = None
_ready = False


def _load_resources() -> None:
    global _model, _collection, _ready
    print(f"Loading embedding model ({MODEL_NAME})...", flush=True)
    from sentence_transformers import SentenceTransformer
    import chromadb

    _model = SentenceTransformer(MODEL_NAME)

    print("Opening ChromaDB index...", flush=True)
    client = chromadb.PersistentClient(path=str(INDEX_DIR))
    try:
        _collection = client.get_collection("knowledge")
        print(f"Index ready: {_collection.count()} chunks", flush=True)
    except Exception as e:
        print(f"[WARN] No index found ({e}). Run: python rag/index.py", flush=True)

    _ready = True
    print(f"Server ready on http://{HOST}:{PORT}", flush=True)


# ---------------------------------------------------------------------------
# Request handler
# ---------------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # suppress default access log
        pass

    def _send_json(self, status: int, data) -> None:
        body = json.dumps(data, indent=2).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        params = urllib.parse.parse_qs(parsed.query)

        if parsed.path == "/health":
            self._send_json(200, {
                "status": "ok" if _ready else "loading",
                "model": MODEL_NAME,
                "chunks": _collection.count() if _collection else 0,
            })

        elif parsed.path == "/query":
            if not _ready:
                self._send_json(503, {"error": "Server not ready yet"})
                return
            if _collection is None:
                self._send_json(503, {"error": "No index. Run: python rag/index.py"})
                return

            question = " ".join(params.get("q", [""])).strip()
            if not question:
                self._send_json(400, {"error": "Missing ?q= parameter"})
                return

            top_k = int(" ".join(params.get("top_k", ["5"])))
            fmt = " ".join(params.get("fmt", ["text"]))

            query_text = "query: " + question
            embedding = _model.encode([query_text], normalize_embeddings=True)[0].tolist()

            results = _collection.query(
                query_embeddings=[embedding],
                n_results=top_k,
            )

            if not results["documents"] or not results["documents"][0]:
                self._send_json(200, {"query": question, "results": []})
                return

            entries = []
            for doc, meta, dist in zip(
                results["documents"][0],
                results["metadatas"][0],
                results["distances"][0],
            ):
                entries.append({
                    "text": doc,
                    "source": meta.get("source", "unknown") if meta else "unknown",
                    "relevance": round(1 - dist, 3),
                })

            if fmt == "json":
                self._send_json(200, {"query": question, "results": entries})
            else:
                lines = [f"Query: {question}", f"Results: {len(entries)}", ""]
                for i, e in enumerate(entries):
                    lines.append(f"--- [{i+1}] relevance={e['relevance']} source={e['source']} ---")
                    lines.append(e["text"][:500])
                    lines.append("")
                body = "\n".join(lines).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        else:
            self._send_json(404, {"error": "Not found"})

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path == "/shutdown":
            self._send_json(200, {"status": "shutting down"})
            threading.Thread(target=self.server.shutdown, daemon=True).start()
        else:
            self._send_json(404, {"error": "Not found"})


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    # Load model + index in a background thread so the HTTP server binds
    # immediately and can serve /health while loading.
    threading.Thread(target=_load_resources, daemon=True).start()

    server = HTTPServer((HOST, PORT), Handler)
    print(f"RAG server starting on http://{HOST}:{PORT} (loading model...)", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nServer stopped.")


if __name__ == "__main__":
    main()
