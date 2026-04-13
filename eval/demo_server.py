#!/usr/bin/env python3
"""Voice Anonymization Demo Server

Localhost web UI for testing voice anonymization interactively.
Upload audio → anonymize → play before / after → view metrics → send to Telegram.

Usage:
    cd /path/to/Agents-Voice
    python eval/demo_server.py              # http://127.0.0.1:5050
    python eval/demo_server.py --port 8080  # custom port
"""

import json
import os
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path

from flask import Flask, jsonify, render_template, request, send_from_directory

ROOT = Path(__file__).resolve().parent.parent  # Agents-Voice/
UPLOAD_DIR = ROOT / "eval" / "results" / "uploads"
ANON_DIR = ROOT / "eval" / "results" / "anonymized"
UPLOAD_DIR.mkdir(parents=True, exist_ok=True)
ANON_DIR.mkdir(parents=True, exist_ok=True)

app = Flask(
    __name__,
    template_folder=str(Path(__file__).parent / "templates"),
)


# ---------- Pages ----------------------------------------------------------

@app.route("/")
def index():
    return render_template("index.html")


# ---------- Audio serving --------------------------------------------------

@app.route("/audio/uploads/<path:filename>")
def serve_upload(filename):
    return send_from_directory(str(UPLOAD_DIR), filename)


@app.route("/audio/anonymized/<path:filename>")
def serve_anonymized(filename):
    return send_from_directory(str(ANON_DIR), filename)


# ---------- API: upload ----------------------------------------------------

@app.route("/api/upload", methods=["POST"])
def upload_audio():
    if "audio" not in request.files:
        return jsonify({"error": "No audio file provided"}), 400
    f = request.files["audio"]
    if not f.filename:
        return jsonify({"error": "No file selected"}), 400

    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    safe_name = f"{ts}_{f.filename}"
    dest = UPLOAD_DIR / safe_name
    f.save(str(dest))

    return jsonify({
        "id": ts,
        "filename": safe_name,
        "input_path": str(dest),
    })


# ---------- API: anonymize ------------------------------------------------

@app.route("/api/anonymize", methods=["POST"])
def anonymize():
    data = request.get_json(force=True)
    input_path = data.get("input_path", "")
    if not input_path or not Path(input_path).exists():
        return jsonify({"error": "Input file not found"}), 400

    out_name = f"anon_{Path(input_path).name}"
    out_path = ANON_DIR / out_name

    # Try the Rust anonymization binary
    pipeline_status = "placeholder"
    try:
        result = subprocess.run(
            [
                str(ROOT / "target" / "release" / "claudir"),
                "anonymize",
                "--input", str(input_path),
                "--output", str(out_path),
            ],
            capture_output=True, text=True, timeout=60,
        )
        if result.returncode == 0 and out_path.exists():
            pipeline_status = "real"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    if pipeline_status == "placeholder":
        # Copy original so the UI still works for demo purposes
        shutil.copy2(input_path, out_path)

    return jsonify({
        "input_path": str(input_path),
        "output_path": str(out_path),
        "output_filename": out_name,
        "input_filename": Path(input_path).name,
        "pipeline_status": pipeline_status,
    })


# ---------- API: metrics ---------------------------------------------------

@app.route("/api/metrics", methods=["POST"])
def compute_metrics():
    data = request.get_json(force=True)
    input_path = data.get("input_path", "")
    output_path = data.get("output_path", "")

    if not input_path or not output_path:
        return jsonify({"error": "Missing input_path or output_path"}), 400

    try:
        result = subprocess.run(
            [
                sys.executable, str(ROOT / "eval" / "metrics.py"),
                "-i", output_path,
                "-r", input_path,
                "-o", "/dev/null",
            ],
            capture_output=True, text=True, timeout=120, cwd=str(ROOT),
        )
        # Parse the verdict block from stdout
        metrics: dict = {}
        for line in result.stdout.strip().splitlines():
            if line.startswith("MOS:"):
                metrics["mos"] = float(line.split(":")[1].split("[")[0].replace("%", "").strip())
                metrics["mos_pass"] = "PASS" in line
            elif line.startswith("EER:"):
                metrics["eer"] = float(line.split(":")[1].split("[")[0].replace("%", "").strip())
                metrics["eer_pass"] = "PASS" in line
            elif line.startswith("SPEAKER_SIM:"):
                metrics["speaker_similarity"] = float(
                    line.split(":")[1].split("[")[0].strip())
                metrics["speaker_sim_pass"] = "PASS" in line
            elif line.startswith("WER:"):
                metrics["wer"] = float(line.split(":")[1].split("[")[0].replace("%", "").strip())
                metrics["wer_pass"] = "PASS" in line
            elif line.startswith("VERDICT:"):
                metrics["verdict"] = line.split(":")[1].strip()

        return jsonify({"metrics": metrics, "raw_stdout": result.stdout,
                        "raw_stderr": result.stderr})
    except subprocess.TimeoutExpired:
        return jsonify({"error": "Evaluation timed out (120 s)"}), 504
    except Exception as exc:
        return jsonify({"error": str(exc)}), 500


# ---------- API: Telegram notify -------------------------------------------

@app.route("/api/notify", methods=["POST"])
def notify():
    data = request.get_json(force=True)
    verdict_text = data.get("verdict", "No verdict provided")

    token = os.environ.get("EVAL_BOT_TOKEN") or os.environ.get("ATLAS_BOT_TOKEN")
    chat_id = os.environ.get("EVAL_CHAT_ID", "-1003399442526")
    owner_id = os.environ.get("OWNER_ID", "8202621898")

    if not token:
        return jsonify({"error": "No bot token (set EVAL_BOT_TOKEN)"}), 400

    import requests as _req
    text = (
        "Voice Anonymization Eval\n\n"
        f"```\n{verdict_text}\n```\n\n"
        f"[owner](tg://user?id={owner_id}) — review at localhost:5050"
    )
    resp = _req.post(
        f"https://api.telegram.org/bot{token}/sendMessage",
        json={"chat_id": chat_id, "text": text, "parse_mode": "Markdown"},
        timeout=10,
    )
    return jsonify({"ok": resp.ok, "detail": resp.json()})


# ---------- API: list existing results -------------------------------------

@app.route("/api/results")
def list_results():
    uploads = sorted(UPLOAD_DIR.glob("*.*"), reverse=True)[:20]
    anons = sorted(ANON_DIR.glob("*.*"), reverse=True)[:20]
    return jsonify({
        "uploads": [f.name for f in uploads],
        "anonymized": [f.name for f in anons],
    })


# ---------- API: metrics history -------------------------------------------

@app.route("/api/history")
def metrics_history():
    jsonl = ROOT / "eval" / "results" / "metrics.jsonl"
    if not jsonl.exists():
        return jsonify([])
    entries = []
    for line in jsonl.read_text().strip().splitlines():
        try:
            entries.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return jsonify(entries[-50:])  # last 50


# ---------- Main -----------------------------------------------------------

if __name__ == "__main__":
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5050)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()
    print(f"Demo server → http://{args.host}:{args.port}")
    app.run(host=args.host, port=args.port, debug=True)
