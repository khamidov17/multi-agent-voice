#!/usr/bin/env python3
"""Build the RAG knowledge index from curated sources.

Reads all files from knowledge/{papers,repos,links,docs}, chunks them,
generates embeddings, and stores in a ChromaDB collection.

Supports incremental indexing: only files whose content has changed
(by SHA-256 hash) are re-processed. Unchanged files keep their existing
embeddings. This makes re-indexing after adding a single paper very fast.

Usage:
  python rag/index.py              # Incremental update (only changed files)
  python rag/index.py --full       # Force full rebuild
  python rag/index.py --stats      # Show index statistics

Dependencies:
  pip install sentence-transformers chromadb pymupdf trafilatura
"""

import argparse
import hashlib
import json
import os
import sys
import time
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
KNOWLEDGE_DIR = SCRIPT_DIR / "knowledge"
INDEX_DIR = SCRIPT_DIR / "index"
SOURCES_FILE = SCRIPT_DIR / "sources.yaml"
FILE_HASHES_FILE = INDEX_DIR / ".file_hashes.json"
VERSION_FILE = INDEX_DIR / ".version.json"
MAX_VERSIONS = 3

# Chunking parameters
CHUNK_SIZE = 1000       # target characters per chunk
CHUNK_OVERLAP = 200     # overlap carried into next chunk (sentence-boundary aware)

# Sentence boundary split tokens, in priority order
SENTENCE_BOUNDARIES = ["\n\n", "\n", ". ", "? ", "! ", "; "]


# ---------------------------------------------------------------------------
# Text extraction
# ---------------------------------------------------------------------------

def extract_text_from_pdf(pdf_path: str) -> str:
    """Extract text from a PDF file using PyMuPDF (fitz)."""
    try:
        import fitz  # pymupdf
        doc = fitz.open(pdf_path)
        pages = []
        for page in doc:
            pages.append(page.get_text())
        doc.close()
        return "\n".join(pages).strip()
    except ImportError:
        print(f"  [WARN] pymupdf not installed, skipping PDF: {pdf_path}", file=sys.stderr)
        return ""
    except Exception as e:
        print(f"  [WARN] Failed to read PDF {pdf_path}: {e}", file=sys.stderr)
        return ""


def extract_text_from_url(url: str) -> str:
    """Fetch and extract text from a URL.

    - GitHub blob URLs → converted to raw.githubusercontent.com
    - arXiv abstract URLs → converted to /abs/ API page (HTML is clean)
    - Everything else → trafilatura (best-in-class boilerplate removal)
    - Fallback → regex HTML strip
    """
    import urllib.request
    import re

    # Normalise GitHub blob URLs to raw content
    if "github.com" in url and "/blob/" in url:
        url = url.replace("github.com", "raw.githubusercontent.com").replace("/blob/", "/")

    # For arXiv, prefer the abstract page (trafilatura handles it well)
    # e.g. https://arxiv.org/pdf/2203.12345 → https://arxiv.org/abs/2203.12345
    arxiv_pdf_match = re.match(r"https?://arxiv\.org/pdf/(\d+\.\d+)", url)
    if arxiv_pdf_match:
        url = f"https://arxiv.org/abs/{arxiv_pdf_match.group(1)}"

    try:
        req = urllib.request.Request(url, headers={"User-Agent": "RAG-Indexer/1.0"})
        with urllib.request.urlopen(req, timeout=20) as resp:
            html = resp.read().decode("utf-8", errors="replace")
    except Exception as e:
        print(f"  [WARN] Failed to fetch URL {url}: {e}", file=sys.stderr)
        return ""

    # Try trafilatura first (best quality HTML extraction)
    try:
        import trafilatura
        extracted = trafilatura.extract(html, include_comments=False, include_tables=True)
        if extracted and len(extracted) > 200:
            return extracted[:80000]
    except ImportError:
        pass
    except Exception:
        pass

    # Fallback: naive HTML tag stripping
    text = re.sub(r"<[^>]+>", " ", html)
    text = re.sub(r"\s+", " ", text).strip()
    return text[:80000]


# ---------------------------------------------------------------------------
# Sentence-boundary chunking
# ---------------------------------------------------------------------------

def _split_on_boundaries(text: str) -> list[str]:
    """Split text into natural sentences/paragraphs using boundary tokens."""
    # Try boundaries from most coarse to finest
    for boundary in SENTENCE_BOUNDARIES:
        parts = text.split(boundary)
        if len(parts) > 1:
            # Re-attach boundary token to keep punctuation with preceding sentence
            rejoined = []
            for i, part in enumerate(parts):
                if i < len(parts) - 1:
                    rejoined.append(part + boundary)
                else:
                    rejoined.append(part)
            return [s for s in rejoined if s.strip()]
    return [text] if text.strip() else []


def chunk_text(text: str, source: str, chunk_size: int = CHUNK_SIZE, overlap: int = CHUNK_OVERLAP) -> list[dict]:
    """Split text into overlapping chunks respecting sentence boundaries.

    Strategy:
    1. Split text on sentence/paragraph boundaries first.
    2. Greedily merge small pieces until we approach chunk_size.
    3. When adding the next piece would exceed chunk_size, emit the current
       chunk and carry the last `overlap` characters into the next chunk as
       a seed — but always start the new chunk at a sentence boundary.
    """
    if not text.strip():
        return []

    sentences = _split_on_boundaries(text)
    if not sentences:
        return []

    chunks = []
    current_parts: list[str] = []
    current_len = 0
    chunk_idx = 0

    for sentence in sentences:
        sentence_len = len(sentence)

        # If a single sentence exceeds chunk_size, force-split it by characters
        # but still try to land on a word boundary.
        if sentence_len > chunk_size:
            # Flush what we have first
            if current_parts:
                chunk_text_val = "".join(current_parts).strip()
                if chunk_text_val:
                    content_hash = hashlib.md5(chunk_text_val.encode()).hexdigest()
                    chunks.append({
                        "text": chunk_text_val,
                        "source": source,
                        "chunk_index": chunk_idx,
                        "content_hash": content_hash,
                    })
                    chunk_idx += 1
                current_parts = []
                current_len = 0

            # Hard-split the long sentence at word boundaries
            words = sentence.split(" ")
            word_buf: list[str] = []
            word_len = 0
            for word in words:
                word_with_space = word + " "
                if word_len + len(word_with_space) > chunk_size and word_buf:
                    piece = "".join(word_buf).strip()
                    if piece:
                        content_hash = hashlib.md5(piece.encode()).hexdigest()
                        chunks.append({
                            "text": piece,
                            "source": source,
                            "chunk_index": chunk_idx,
                            "content_hash": content_hash,
                        })
                        chunk_idx += 1
                    # Start overlap: keep last overlap-worth of words
                    overlap_text = piece[-overlap:] if len(piece) > overlap else piece
                    word_buf = [overlap_text]
                    word_len = len(overlap_text)
                word_buf.append(word_with_space)
                word_len += len(word_with_space)
            if word_buf:
                piece = "".join(word_buf).strip()
                if piece:
                    current_parts = [piece]
                    current_len = len(piece)
            continue

        if current_len + sentence_len > chunk_size and current_parts:
            # Emit current chunk
            chunk_text_val = "".join(current_parts).strip()
            if chunk_text_val:
                content_hash = hashlib.md5(chunk_text_val.encode()).hexdigest()
                chunks.append({
                    "text": chunk_text_val,
                    "source": source,
                    "chunk_index": chunk_idx,
                    "content_hash": content_hash,
                })
                chunk_idx += 1

            # Seed next chunk with last `overlap` chars of current chunk
            seed = chunk_text_val[-overlap:] if len(chunk_text_val) > overlap else chunk_text_val
            current_parts = [seed] if seed else []
            current_len = len(seed)

        current_parts.append(sentence)
        current_len += sentence_len

    # Flush remainder
    if current_parts:
        chunk_text_val = "".join(current_parts).strip()
        if chunk_text_val:
            content_hash = hashlib.md5(chunk_text_val.encode()).hexdigest()
            chunks.append({
                "text": chunk_text_val,
                "source": source,
                "chunk_index": chunk_idx,
                "content_hash": content_hash,
            })

    return chunks


# ---------------------------------------------------------------------------
# File hash tracking (incremental indexing)
# ---------------------------------------------------------------------------

def load_file_hashes() -> dict:
    """Load persisted per-file content hashes from disk."""
    if FILE_HASHES_FILE.exists():
        try:
            return json.loads(FILE_HASHES_FILE.read_text())
        except Exception:
            pass
    return {}


def save_file_hashes(hashes: dict) -> None:
    os.makedirs(INDEX_DIR, exist_ok=True)
    FILE_HASHES_FILE.write_text(json.dumps(hashes, indent=2))


def file_content_hash(path: Path) -> str:
    """SHA-256 of a file's bytes."""
    h = hashlib.sha256()
    h.update(path.read_bytes())
    return h.hexdigest()


def url_content_hash(text: str) -> str:
    """SHA-256 of fetched URL text (used as the 'file' hash for URL sources)."""
    return hashlib.sha256(text.encode()).hexdigest()


# ---------------------------------------------------------------------------
# Document collection
# ---------------------------------------------------------------------------

def collect_documents(force_full: bool = False) -> tuple[list[dict], dict]:
    """Collect documents from knowledge/ subfolders.

    Returns (all_new_chunks, updated_file_hashes).
    When force_full=False, skips files whose hash matches the cached hash.
    The caller is responsible for merging with existing index chunks.
    """
    old_hashes = {} if force_full else load_file_hashes()
    new_hashes: dict = dict(old_hashes)
    changed_sources: set[str] = set()
    all_chunks: list[dict] = []

    # --- Papers (PDFs) ---
    papers_dir = KNOWLEDGE_DIR / "papers"
    if papers_dir.is_dir():
        for pdf in sorted(papers_dir.glob("*.pdf")):
            fhash = file_content_hash(pdf)
            key = f"paper:{pdf.name}"
            if not force_full and old_hashes.get(key) == fhash:
                print(f"  [skip] paper: {pdf.name} (unchanged)")
                continue
            print(f"  Reading paper: {pdf.name}")
            text = extract_text_from_pdf(str(pdf))
            if text:
                chunks = chunk_text(text, source=key)
                all_chunks.extend(chunks)
                new_hashes[key] = fhash
                changed_sources.add(key)
                print(f"    → {len(chunks)} chunks")

    # --- Repos ---
    repos_dir = KNOWLEDGE_DIR / "repos"
    code_extensions = {".py", ".rs", ".js", ".ts", ".md", ".txt", ".yaml", ".yml", ".toml", ".cfg", ".sh"}
    if repos_dir.is_dir():
        for repo_dir in sorted(repos_dir.iterdir()):
            if not repo_dir.is_dir() or repo_dir.name.startswith("."):
                continue
            print(f"  Reading repo: {repo_dir.name}")
            repo_chunks = 0
            for code_file in sorted(repo_dir.rglob("*")):
                if code_file.suffix.lower() not in code_extensions or not code_file.is_file():
                    continue
                try:
                    fhash = file_content_hash(code_file)
                    rel_path = code_file.relative_to(repos_dir)
                    key = f"repo:{rel_path}"
                    if not force_full and old_hashes.get(key) == fhash:
                        continue
                    text = code_file.read_text(encoding="utf-8", errors="replace")
                    if len(text) > 100:
                        chunks = chunk_text(text, source=key)
                        all_chunks.extend(chunks)
                        new_hashes[key] = fhash
                        changed_sources.add(key)
                        repo_chunks += len(chunks)
                except Exception:
                    continue
            if repo_chunks:
                print(f"    → {repo_chunks} new/changed chunks")

    # --- Links (URLs) ---
    links_dir = KNOWLEDGE_DIR / "links"
    if links_dir.is_dir():
        for link_file in sorted(links_dir.glob("*.txt")):
            print(f"  Reading links: {link_file.name}")
            urls = [
                line.strip()
                for line in link_file.read_text().splitlines()
                if line.strip() and not line.startswith("#")
            ]
            for url in urls:
                key = f"url:{url}"
                print(f"    Fetching: {url[:80]}...")
                text = extract_text_from_url(url)
                if not text:
                    continue
                uhash = url_content_hash(text)
                if not force_full and old_hashes.get(key) == uhash:
                    print(f"      [skip] unchanged")
                    continue
                chunks = chunk_text(text, source=key)
                all_chunks.extend(chunks)
                new_hashes[key] = uhash
                changed_sources.add(key)
                print(f"      → {len(chunks)} chunks")

    # --- Docs ---
    docs_dir = KNOWLEDGE_DIR / "docs"
    doc_extensions = {".txt", ".md", ".rst", ".py", ".rs", ".yaml", ".json"}
    if docs_dir.is_dir():
        for doc_file in sorted(docs_dir.rglob("*")):
            if not doc_file.is_file() or doc_file.suffix.lower() not in doc_extensions:
                continue
            try:
                fhash = file_content_hash(doc_file)
                key = f"doc:{doc_file.name}"
                if not force_full and old_hashes.get(key) == fhash:
                    print(f"  [skip] doc: {doc_file.name} (unchanged)")
                    continue
                print(f"  Reading doc: {doc_file.name}")
                text = doc_file.read_text(encoding="utf-8", errors="replace")
                if text.strip():
                    chunks = chunk_text(text, source=key)
                    all_chunks.extend(chunks)
                    new_hashes[key] = fhash
                    changed_sources.add(key)
                    print(f"    → {len(chunks)} chunks")
            except Exception:
                continue

    return all_chunks, new_hashes, changed_sources


# ---------------------------------------------------------------------------
# Deduplication
# ---------------------------------------------------------------------------

def deduplicate_chunks(chunks: list[dict]) -> list[dict]:
    """Remove chunks with identical text content across sources.

    Keeps the first occurrence (by insertion order, i.e. earlier source wins).
    Uses the pre-computed content_hash field.
    """
    seen: set[str] = set()
    result: list[dict] = []
    for chunk in chunks:
        h = chunk["content_hash"]
        if h not in seen:
            seen.add(h)
            result.append(chunk)
    return result


# ---------------------------------------------------------------------------
# Index versioning
# ---------------------------------------------------------------------------

def load_versions() -> list[dict]:
    if VERSION_FILE.exists():
        try:
            return json.loads(VERSION_FILE.read_text())
        except Exception:
            pass
    return []


def save_version(chunk_count: int, sources: dict) -> None:
    versions = load_versions()
    versions.append({
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "chunk_count": chunk_count,
        "source_count": len(sources),
        "sources": sources,
    })
    # Keep last MAX_VERSIONS
    versions = versions[-MAX_VERSIONS:]
    os.makedirs(INDEX_DIR, exist_ok=True)
    VERSION_FILE.write_text(json.dumps(versions, indent=2))


# ---------------------------------------------------------------------------
# Index builder
# ---------------------------------------------------------------------------

def build_index(new_chunks: list[dict], changed_sources: set[str], force_full: bool) -> None:
    """Build or update ChromaDB index."""
    import chromadb
    from sentence_transformers import SentenceTransformer

    os.makedirs(INDEX_DIR, exist_ok=True)
    client = chromadb.PersistentClient(path=str(INDEX_DIR))

    if force_full:
        # Wipe existing collection
        try:
            client.delete_collection("knowledge")
        except Exception:
            pass
        collection = client.create_collection(
            name="knowledge",
            metadata={"hnsw:space": "cosine"},
        )
        docs_to_index = new_chunks
    else:
        # Get or create collection; remove stale entries for changed sources
        try:
            collection = client.get_collection("knowledge")
        except Exception:
            collection = client.create_collection(
                name="knowledge",
                metadata={"hnsw:space": "cosine"},
            )

        if changed_sources:
            # Delete all existing chunks from changed sources so we don't accumulate duplicates
            try:
                existing = collection.get(where={"source": {"$in": list(changed_sources)}})
                if existing["ids"]:
                    collection.delete(ids=existing["ids"])
                    print(f"  Removed {len(existing['ids'])} stale chunks for {len(changed_sources)} changed sources")
            except Exception:
                pass

        docs_to_index = new_chunks

    if not docs_to_index:
        print("No new/changed chunks to index.")
        count = collection.count()
        print(f"Index unchanged: {count} total chunks in {INDEX_DIR}")
        _record_index_state(collection, [])
        return

    print(f"\nLoading embedding model (BAAI/bge-base-en-v1.5, 768-dim)...")
    model = SentenceTransformer("BAAI/bge-base-en-v1.5")

    print(f"Generating embeddings for {len(docs_to_index)} chunks...")
    texts = [d["text"] for d in docs_to_index]
    # BGE models benefit from a query/passage prefix during encoding
    passages = ["passage: " + t for t in texts]
    embeddings = model.encode(passages, show_progress_bar=True, batch_size=32, normalize_embeddings=True)

    batch_size = 500
    for i in range(0, len(docs_to_index), batch_size):
        batch = docs_to_index[i:i + batch_size]
        batch_texts = [d["text"] for d in batch]
        batch_embeddings = embeddings[i:i + batch_size].tolist()
        # Use content_hash as ChromaDB ID so duplicate text across sources
        # won't be double-inserted (upsert semantics).
        batch_ids = [d["content_hash"] for d in batch]
        batch_metadatas = [{"source": d["source"], "chunk_index": d["chunk_index"]} for d in batch]

        collection.upsert(
            ids=batch_ids,
            documents=batch_texts,
            embeddings=batch_embeddings,
            metadatas=batch_metadatas,
        )

    total = collection.count()
    print(f"Index updated: {total} total chunks stored in {INDEX_DIR}")
    _record_index_state(collection, docs_to_index)


def _record_index_state(collection, new_chunks: list[dict]) -> None:
    """Save sources.yaml and version entry after indexing."""
    # Rebuild source summary from ChromaDB metadata
    try:
        all_meta = collection.get()["metadatas"] or []
    except Exception:
        all_meta = [d for d in []]

    sources: dict = {}
    for m in all_meta:
        src = m.get("source", "unknown") if m else "unknown"
        sources[src] = sources.get(src, 0) + 1

    with open(SOURCES_FILE, "w") as f:
        f.write("# RAG Knowledge Sources\n")
        f.write(f"# Built: {time.strftime('%Y-%m-%d %H:%M:%S')}\n")
        f.write(f"# Total chunks: {sum(sources.values())}\n\n")
        for src, count in sorted(sources.items()):
            f.write(f"- {src}: {count} chunks\n")

    save_version(sum(sources.values()), sources)
    print(f"Sources saved to {SOURCES_FILE}")


# ---------------------------------------------------------------------------
# Stats
# ---------------------------------------------------------------------------

def show_stats() -> None:
    """Show index statistics including version history."""
    import chromadb
    client = chromadb.PersistentClient(path=str(INDEX_DIR))
    try:
        collection = client.get_collection("knowledge")
        print(f"Index: {collection.count()} chunks")
        if SOURCES_FILE.exists():
            print(SOURCES_FILE.read_text())
    except Exception:
        print("No index found. Run: python rag/index.py")
        return

    versions = load_versions()
    if versions:
        print("\nVersion history (last 3 builds):")
        for v in versions:
            print(f"  {v['timestamp']}  chunks={v['chunk_count']}  sources={v['source_count']}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Build RAG knowledge index")
    parser.add_argument("--stats", action="store_true", help="Show index statistics")
    parser.add_argument("--full", action="store_true", help="Force full rebuild (ignore cached hashes)")
    args = parser.parse_args()

    if args.stats:
        show_stats()
        return

    print("=" * 60)
    print("RAG Knowledge Indexer")
    print("=" * 60)

    if args.full:
        print("\n[FULL REBUILD] Ignoring cached hashes.")

    print(f"\nCollecting documents from {KNOWLEDGE_DIR}...")
    new_chunks, new_hashes, changed_sources = collect_documents(force_full=args.full)

    if not new_chunks and not args.full:
        print("\nAll sources unchanged. Index is up to date.")
        show_stats()
        return

    # Deduplicate by content hash (cross-source dedup)
    before = len(new_chunks)
    new_chunks = deduplicate_chunks(new_chunks)
    deduped = before - len(new_chunks)
    if deduped:
        print(f"Deduplication: removed {deduped} duplicate chunks ({before} → {len(new_chunks)})")

    unique_sources = len(set(d["source"] for d in new_chunks))
    print(f"\nNew/changed: {len(new_chunks)} chunks from {unique_sources} sources")

    build_index(new_chunks, changed_sources, force_full=args.full)
    save_file_hashes(new_hashes)

    print("\nDone! Agents can now query with: python rag/query.py \"your question\"")


if __name__ == "__main__":
    main()
