#!/usr/bin/env python3
"""Generic evaluation runner — reads eval_config.yaml and runs all tests.

Sentinel calls this instead of hardcoded metric scripts.
Supports any project type (audio, web, API, code).

Usage:
  python3 rag/eval_runner.py                              # run all required tests
  python3 rag/eval_runner.py --all                        # run all tests including optional
  python3 rag/eval_runner.py --test "EER"                 # run specific test
  python3 rag/eval_runner.py --config path/to/config.yaml # custom config
  python3 rag/eval_runner.py --vars '{"anon_dir": "/path/to/output"}'  # pass variables

Example configs:

  # Voice anonymization
  tests:
    - name: EER
      command: "cd metrics && python3 run_eval.py --metrics eer --anon-dir {anon_dir}"
      threshold: ">= 30"

  # Web project
  tests:
    - name: Server Health
      command: "curl -s -o /dev/null -w '%{http_code}' http://localhost:8080/health"
      threshold: "== 200"
    - name: Unit Tests
      command: "cd project && pytest tests/ -q"
      threshold: "exit_code == 0"
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

import yaml

DEFAULT_CONFIG = Path(__file__).parent.parent / "data" / "shared" / "eval_config.yaml"


def load_config(config_path: str) -> dict:
    with open(config_path) as f:
        return yaml.safe_load(f)


def substitute_vars(command: str, variables: dict) -> str:
    """Replace {var_name} placeholders with actual values."""
    for key, val in variables.items():
        command = command.replace(f"{{{key}}}", str(val))
    # Check for unresolved placeholders
    unresolved = re.findall(r'\{(\w+)\}', command)
    return command, unresolved


def evaluate_threshold(output: str, exit_code: int, threshold: str) -> tuple:
    """Check if test output meets threshold. Returns (passed: bool, actual_value: str)."""
    threshold = threshold.strip()

    # Exit code check
    if threshold.startswith("exit_code"):
        expected = int(threshold.split("==")[1].strip())
        return exit_code == expected, f"exit_code={exit_code}"

    # String equality
    if threshold.startswith("=="):
        expected = threshold[2:].strip().strip("'\"")
        actual = output.strip()
        return actual == expected, actual

    # Numeric comparison
    # Extract number from output (first float found)
    numbers = re.findall(r'[-+]?\d*\.?\d+', output)
    if not numbers:
        return False, f"no number found in output: {output[:100]}"

    actual = float(numbers[0])

    if threshold.startswith(">="):
        target = float(threshold[2:].strip())
        return actual >= target, f"{actual}"
    elif threshold.startswith("<="):
        target = float(threshold[2:].strip())
        return actual <= target, f"{actual}"
    elif threshold.startswith(">"):
        target = float(threshold[1:].strip())
        return actual > target, f"{actual}"
    elif threshold.startswith("<"):
        target = float(threshold[1:].strip())
        return actual < target, f"{actual}"

    return False, f"unknown threshold format: {threshold}"


def run_test(test: dict, variables: dict, timeout: int = 300) -> dict:
    """Run a single test and return result."""
    name = test["name"]
    command = test["command"]
    threshold = test.get("threshold", "exit_code == 0")

    command, unresolved = substitute_vars(command, variables)
    if unresolved:
        return {
            "name": name,
            "passed": False,
            "value": "N/A",
            "error": f"Unresolved variables: {unresolved}. Pass them with --vars",
            "skipped": True,
        }

    # Security: validate command doesn't contain shell injection patterns
    # eval_config.yaml is trusted (only Nova/owner can write it), but defense-in-depth
    dangerous = ["&&", "||", "|", ";", "`", "$(", "${", ">", "<", "\\n"]
    # Allow && and | for piped commands in eval configs, but block backticks and $()
    shell_inject = ["`", "$(", "${"]
    for pattern in shell_inject:
        if pattern in command:
            return {
                "name": name, "passed": False, "value": "BLOCKED",
                "error": f"Command contains blocked pattern: {pattern}",
                "skipped": True,
            }

    try:
        result = subprocess.run(
            command, shell=True, capture_output=True, text=True,
            timeout=timeout, cwd=str(Path(__file__).parent.parent),
        )
        output = result.stdout.strip()
        if not output and result.stderr.strip():
            output = result.stderr.strip()

        passed, actual = evaluate_threshold(output, result.returncode, threshold)

        return {
            "name": name,
            "passed": passed,
            "value": actual,
            "threshold": threshold,
            "exit_code": result.returncode,
            "output_preview": output[:200] if output else "",
            "skipped": False,
        }
    except subprocess.TimeoutExpired:
        return {"name": name, "passed": False, "value": "TIMEOUT", "error": f"Timed out after {timeout}s", "skipped": False}
    except Exception as e:
        return {"name": name, "passed": False, "value": "ERROR", "error": str(e), "skipped": False}


def main():
    parser = argparse.ArgumentParser(description="Generic evaluation runner")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG), help="Path to eval_config.yaml")
    parser.add_argument("--all", action="store_true", help="Run all tests including optional")
    parser.add_argument("--test", help="Run specific test by name")
    parser.add_argument("--vars", default="{}", help="JSON string of variables to substitute")
    parser.add_argument("--timeout", type=int, default=300, help="Per-test timeout in seconds")
    parser.add_argument("--json", action="store_true", help="Output as JSON")
    args = parser.parse_args()

    config = load_config(args.config)
    variables = json.loads(args.vars)
    tests = config.get("tests", [])

    if args.test:
        tests = [t for t in tests if t["name"].lower() == args.test.lower()]
        if not tests:
            print(f"Test '{args.test}' not found in config")
            sys.exit(1)

    if not args.all:
        tests = [t for t in tests if t.get("required", True)]

    start = time.time()
    results = []
    for test in tests:
        print(f"Running: {test['name']}...", file=sys.stderr)
        result = run_test(test, variables, args.timeout)
        results.append(result)

    duration = round(time.time() - start, 1)

    # Overall verdict
    non_skipped = [r for r in results if not r.get("skipped")]
    all_passed = all(r["passed"] for r in non_skipped) if non_skipped else False
    any_failed = any(not r["passed"] and not r.get("skipped") for r in results)

    report = {
        "project": config.get("project", "unknown"),
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "overall": "PASS" if all_passed else ("FAIL" if any_failed else "PARTIAL"),
        "duration_secs": duration,
        "results": results,
    }

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"\n{'='*60}")
        print(f"EVALUATION REPORT — {config.get('project', 'unknown')}")
        print(f"{'='*60}")
        for r in results:
            if r.get("skipped"):
                sym = "SKIP"
            elif r["passed"]:
                sym = "PASS"
            else:
                sym = "FAIL"
            print(f"  [{sym:4s}] {r['name']}: {r['value']}  (threshold: {r.get('threshold', 'N/A')})")
            if r.get("error"):
                print(f"         Error: {r['error']}")
        print(f"\n  OVERALL: {report['overall']}")
        print(f"  Duration: {duration}s")
        print(f"{'='*60}")

    # Save report
    report_path = Path(__file__).parent.parent / "eval_results" / "eval_report.json"
    os.makedirs(report_path.parent, exist_ok=True)
    with open(report_path, "w") as f:
        json.dump(report, f, indent=2)

    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    main()
