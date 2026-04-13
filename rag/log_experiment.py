#!/usr/bin/env python3
"""Log experiment results for the team's shared lab notebook.

Sentinel calls this after every evaluation run. All agents can read
the log to learn from past attempts before planning new work.

Usage:
  # Log a result (with optional RAG sources consulted before the decision)
  python rag/log_experiment.py --task "OHNN pipeline v1" --method "ECAPA-TDNN + HiFi-GAN" \
    --metrics '{"eer": 25.3, "wer": 12.1, "pmos": 3.8}' --verdict FAIL \
    --notes "EER below 30% threshold, need stronger embedding pool" \
    --sources '["how does ECAPA-TDNN work?", "speaker anonymization baselines"]'

  # View recent experiments
  python rag/log_experiment.py --view
  python rag/log_experiment.py --view --last 20

  # Search experiments
  python rag/log_experiment.py --search "ECAPA"

  # Summary (methods tried + what worked/failed)
  python rag/log_experiment.py --summary
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path

LOG_FILE = Path(__file__).parent.parent / "data" / "shared" / "experiments.jsonl"


def log_experiment(
    task: str,
    method: str,
    metrics: dict,
    verdict: str,
    notes: str = "",
    sources: list | None = None,
) -> None:
    """Append an experiment entry to the shared log.

    Args:
        task:    Short name for the task (e.g. "OHNN pipeline v1").
        method:  Description of the approach (e.g. "ECAPA-TDNN + HiFi-GAN").
        metrics: Dict of metric name → value.
        verdict: "PASS" or "FAIL".
        notes:   Free-text notes.
        sources: List of RAG queries made before this decision, so future
                 agents can see what knowledge was consulted.  Pass an empty
                 list (default) when no RAG was used.
    """
    entry = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "task": task,
        "method": method,
        "metrics": metrics,
        "verdict": verdict.upper(),
        "notes": notes,
        "rag_sources": sources if sources is not None else [],
    }
    os.makedirs(LOG_FILE.parent, exist_ok=True)
    with open(LOG_FILE, "a") as f:
        f.write(json.dumps(entry) + "\n")
    rag_note = f" (consulted {len(entry['rag_sources'])} RAG queries)" if entry["rag_sources"] else ""
    print(f"Logged: {verdict} — {task} ({method}){rag_note}")


def view_experiments(last_n: int = 10) -> str:
    """View recent experiments."""
    if not LOG_FILE.exists():
        return "No experiments logged yet."

    entries = []
    with open(LOG_FILE) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

    if not entries:
        return "No experiments logged yet."

    entries = entries[-last_n:]
    lines = [f"Last {len(entries)} experiments:", ""]
    for e in entries:
        verdict_sym = "PASS" if e["verdict"] == "PASS" else "FAIL"
        metrics_str = " ".join(f"{k}={v}" for k, v in e.get("metrics", {}).items())
        lines.append(f"[{verdict_sym}] {e['timestamp'][:16]} | {e['task']}")
        lines.append(f"       Method: {e['method']}")
        lines.append(f"       Metrics: {metrics_str}")
        if e.get("notes"):
            lines.append(f"       Notes: {e['notes']}")
        rag_sources = e.get("rag_sources", [])
        if rag_sources:
            lines.append(f"       RAG queries: {', '.join(repr(s) for s in rag_sources)}")
        lines.append("")

    return "\n".join(lines)


def search_experiments(query: str) -> str:
    """Search experiments by keyword."""
    if not LOG_FILE.exists():
        return "No experiments logged yet."

    query_lower = query.lower()
    matches = []
    with open(LOG_FILE) as f:
        for line in f:
            if query_lower in line.lower():
                try:
                    matches.append(json.loads(line.strip()))
                except json.JSONDecodeError:
                    continue

    if not matches:
        return f"No experiments matching '{query}'."

    lines = [f"Found {len(matches)} experiments matching '{query}':", ""]
    for e in matches:
        metrics_str = " ".join(f"{k}={v}" for k, v in e.get("metrics", {}).items())
        lines.append(f"[{e['verdict']}] {e['task']} — {e['method']}")
        lines.append(f"       {metrics_str}")
        if e.get("notes"):
            lines.append(f"       {e['notes']}")
        rag_sources = e.get("rag_sources", [])
        if rag_sources:
            lines.append(f"       RAG queries: {', '.join(repr(s) for s in rag_sources)}")
        lines.append("")

    return "\n".join(lines)


def summary() -> str:
    """Generate a summary of all methods tried."""
    if not LOG_FILE.exists():
        return "No experiments logged yet."

    entries = []
    with open(LOG_FILE) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

    if not entries:
        return "No experiments logged yet."

    total = len(entries)
    passed = sum(1 for e in entries if e["verdict"] == "PASS")
    failed = total - passed

    # Count unique RAG queries consulted across all experiments
    all_rag_queries: list[str] = []
    for e in entries:
        all_rag_queries.extend(e.get("rag_sources", []))
    unique_rag = len(set(all_rag_queries))

    # Group by method
    methods: dict = {}
    for e in entries:
        m = e["method"]
        if m not in methods:
            methods[m] = {"pass": 0, "fail": 0, "last_metrics": {}}
        if e["verdict"] == "PASS":
            methods[m]["pass"] += 1
        else:
            methods[m]["fail"] += 1
        methods[m]["last_metrics"] = e.get("metrics", {})

    lines = [
        "EXPERIMENT SUMMARY",
        f"Total: {total} experiments ({passed} PASS, {failed} FAIL)",
        f"RAG coverage: {len(all_rag_queries)} total queries, {unique_rag} unique topics consulted",
        "",
        "Methods tried:",
    ]
    for method, stats in sorted(methods.items()):
        status = "worked" if stats["pass"] > 0 else "NEVER passed"
        metrics_str = " ".join(f"{k}={v}" for k, v in stats["last_metrics"].items())
        lines.append(f"  [{stats['pass']}P/{stats['fail']}F] {method} — {status}")
        if metrics_str:
            lines.append(f"           Last: {metrics_str}")

    lines.append("")
    lines.append("Agents: before planning new work, check if a method was already tried above.")

    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description="Experiment logger")
    parser.add_argument("--task", help="Task name")
    parser.add_argument("--method", help="Method/approach used")
    parser.add_argument("--metrics", help="JSON string of metrics")
    parser.add_argument("--verdict", choices=["PASS", "FAIL", "pass", "fail"], help="PASS or FAIL")
    parser.add_argument("--notes", default="", help="Additional notes")
    parser.add_argument(
        "--sources",
        default=None,
        help=(
            "JSON array of RAG queries that were made before this decision. "
            "Example: '[\"ECAPA-TDNN architecture\", \"anonymization baselines\"]'"
        ),
    )
    parser.add_argument("--view", action="store_true", help="View recent experiments")
    parser.add_argument("--last", type=int, default=10, help="Number of recent entries to show")
    parser.add_argument("--search", help="Search experiments by keyword")
    parser.add_argument("--summary", action="store_true", help="Show methods summary")
    args = parser.parse_args()

    if args.view:
        print(view_experiments(args.last))
    elif args.search:
        print(search_experiments(args.search))
    elif args.summary:
        print(summary())
    elif args.task and args.method and args.verdict:
        metrics = json.loads(args.metrics) if args.metrics else {}
        sources = json.loads(args.sources) if args.sources else []
        log_experiment(args.task, args.method, metrics, args.verdict, args.notes, sources)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
