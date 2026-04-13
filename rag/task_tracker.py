#!/usr/bin/env python3
"""Task tracker: artifacts manifest + task history.

Agents call this to:
- Record what files were produced during a task (artifacts.json)
- Append completed tasks to history (tasks/history.jsonl)
- Resume from artifacts after a crash

Usage:
  # Record an artifact
  python3 rag/task_tracker.py --artifact "pipeline/anonymize.py" --status done

  # Record in-progress work
  python3 rag/task_tracker.py --artifact "pipeline/vocoder.py" --status in_progress --note "line 47"

  # Complete a task (moves artifacts to history)
  python3 rag/task_tracker.py --complete --task "build anonymization pipeline" --verdict PASS

  # View current artifacts
  python3 rag/task_tracker.py --show

  # View task history
  python3 rag/task_tracker.py --history
"""

import argparse
import json
import os
import time
from pathlib import Path

ARTIFACTS_FILE = Path(__file__).parent.parent / "data" / "shared" / "artifacts.json"
HISTORY_FILE = Path(__file__).parent.parent / "data" / "shared" / "task_history.jsonl"


def load_artifacts() -> dict:
    if ARTIFACTS_FILE.exists():
        with open(ARTIFACTS_FILE) as f:
            return json.load(f)
    return {"session": "", "produced": [], "in_progress": [], "next_action": ""}


def save_artifacts(data: dict):
    os.makedirs(ARTIFACTS_FILE.parent, exist_ok=True)
    with open(ARTIFACTS_FILE, "w") as f:
        json.dump(data, f, indent=2)


def add_artifact(path: str, status: str, note: str = ""):
    data = load_artifacts()
    entry = {"path": path, "status": status, "note": note, "time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    if status == "done":
        data["produced"] = [e for e in data["produced"] if e["path"] != path]
        data["produced"].append(entry)
        data["in_progress"] = [e for e in data["in_progress"] if e["path"] != path]
    else:
        data["in_progress"] = [e for e in data["in_progress"] if e["path"] != path]
        data["in_progress"].append(entry)
    save_artifacts(data)
    print(f"Artifact: {path} [{status}]")


def complete_task(task: str, verdict: str):
    data = load_artifacts()
    entry = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "task": task,
        "verdict": verdict,
        "produced": data.get("produced", []),
        "in_progress": data.get("in_progress", []),
    }
    os.makedirs(HISTORY_FILE.parent, exist_ok=True)
    with open(HISTORY_FILE, "a") as f:
        f.write(json.dumps(entry) + "\n")
    # Clear artifacts for next task
    save_artifacts({"session": "", "produced": [], "in_progress": [], "next_action": ""})
    print(f"Task completed: {task} [{verdict}], {len(entry['produced'])} artifacts archived")


def show_artifacts():
    data = load_artifacts()
    if not data["produced"] and not data["in_progress"]:
        print("No current artifacts.")
        return
    print("Current artifacts:")
    for e in data["produced"]:
        print(f"  [DONE] {e['path']}")
    for e in data["in_progress"]:
        print(f"  [WIP]  {e['path']} — {e.get('note', '')}")


def show_history(last_n: int = 10):
    if not HISTORY_FILE.exists():
        print("No task history.")
        return
    entries = []
    with open(HISTORY_FILE) as f:
        for line in f:
            if line.strip():
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    for e in entries[-last_n:]:
        n = len(e.get("produced", []))
        print(f"[{e['verdict']}] {e['timestamp'][:16]} | {e['task']} ({n} files)")


def main():
    parser = argparse.ArgumentParser(description="Task tracker")
    parser.add_argument("--artifact", help="File path to record")
    parser.add_argument("--status", choices=["done", "in_progress"], help="Artifact status")
    parser.add_argument("--note", default="", help="Note about the artifact")
    parser.add_argument("--complete", action="store_true", help="Complete current task")
    parser.add_argument("--task", help="Task name (with --complete)")
    parser.add_argument("--verdict", help="PASS/FAIL (with --complete)")
    parser.add_argument("--show", action="store_true", help="Show current artifacts")
    parser.add_argument("--history", action="store_true", help="Show task history")
    args = parser.parse_args()

    if args.show:
        show_artifacts()
    elif args.history:
        show_history()
    elif args.complete and args.task:
        complete_task(args.task, args.verdict or "DONE")
    elif args.artifact and args.status:
        add_artifact(args.artifact, args.status, args.note)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
