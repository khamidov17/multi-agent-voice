#!/usr/bin/env bash
# uninstall-phase0.sh — clean removal of the bootstrap-guardian runtime.
#
# Stops the launchd/systemd unit, removes it, and removes <run_dir>.
# Does NOT remove source code under bootstrap-guardian/ (that's a git operation).

set -euo pipefail

env_name="${CLAUDIR_ENV:-dev}"
run_dir_default() {
  if [ "$env_name" = "dev" ]; then
    echo "$HOME/claudir-dev/run"
  else
    echo "/opt/claudir/run"
  fi
}
run_dir="${CLAUDIR_RUN_DIR:-$(run_dir_default)}"

os="$(uname -s)"
if [ "$os" = "Darwin" ]; then
  plist="$HOME/Library/LaunchAgents/com.claudir.bootstrap-guardian.plist"
  if [ -f "$plist" ]; then
    echo "[STOP] launchctl bootout gui/\$(id -u)/com.claudir.bootstrap-guardian"
    launchctl bootout "gui/$(id -u)/com.claudir.bootstrap-guardian" 2>/dev/null || true
    echo "[RM] $plist"
    rm -f "$plist"
  else
    echo "[SKIP] no $plist"
  fi
elif [ "$os" = "Linux" ]; then
  unit="/etc/systemd/system/bootstrap-guardian.service"
  if [ -f "$unit" ]; then
    echo "[STOP] sudo systemctl disable --now bootstrap-guardian"
    sudo systemctl disable --now bootstrap-guardian 2>/dev/null || true
    echo "[RM] $unit"
    sudo rm -f "$unit"
    sudo systemctl daemon-reload
  else
    echo "[SKIP] no $unit"
  fi
else
  echo "Unsupported OS: $os" >&2
  exit 3
fi

if [ -d "$run_dir" ]; then
  echo "[RM] $run_dir (contains guardian.key, nonces.db, audit.jsonl, socket)"
  rm -rf "$run_dir"
fi

echo "Done. Source under bootstrap-guardian/ is intact; remove with git if desired."
