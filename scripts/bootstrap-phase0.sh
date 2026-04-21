#!/usr/bin/env bash
# bootstrap-phase0.sh — one-shot setup for the bootstrap-guardian runtime.
#
# Creates run_dir, generates guardian.key (0400), installs the example
# config to <run_dir>/../guardian.json, registers the launchd/systemd unit,
# and prints the next steps. Idempotent — safe to re-run.
#
# Usage:
#   ./scripts/bootstrap-phase0.sh            # interactive, dev defaults
#   TRIO_ENV=prod ./scripts/bootstrap-phase0.sh
#
# Requires: cargo, whichever of launchctl/systemctl your OS has, openssl.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
env_name="${TRIO_ENV:-dev}"
case "$env_name" in
  dev|prod) ;;
  *)
    echo "TRIO_ENV must be 'dev' or 'prod', got '$env_name'" >&2
    exit 2
    ;;
esac

default_run_dir() {
  if [ "$env_name" = "dev" ]; then
    echo "$HOME/trio-dev/run"
  else
    echo "/opt/trio/run"
  fi
}

run_dir="${TRIO_RUN_DIR:-$(default_run_dir)}"
config_path="${TRIO_GUARDIAN_CONFIG:-$(dirname "$run_dir")/guardian.json}"

echo "=== Trio Phase 0 bootstrap ==="
echo "  env:         $env_name"
echo "  run_dir:     $run_dir"
echo "  config:      $config_path"
echo "  repo root:   $repo_root"
echo ""

# 1. Create run_dir with 0700.
if [ -d "$run_dir" ]; then
  echo "[OK] $run_dir exists"
else
  echo "[CREATE] $run_dir"
  mkdir -p "$run_dir"
fi
chmod 0700 "$run_dir"

# 2. Generate guardian.key if missing.
key_path="$run_dir/guardian.key"
if [ -s "$key_path" ]; then
  echo "[OK] $key_path already exists (leaving as-is)"
else
  echo "[CREATE] $key_path (64 bytes from /dev/urandom)"
  head -c 64 /dev/urandom > "$key_path"
fi
chmod 0400 "$key_path"

# 3. Seed the config if absent.
if [ -s "$config_path" ]; then
  echo "[OK] $config_path already exists (leaving as-is)"
else
  example="$repo_root/bootstrap-guardian/deploy/guardian.example.json"
  if [ ! -f "$example" ]; then
    echo "ERROR: expected example at $example — did you run from the repo root?" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$config_path")"
  cp "$example" "$config_path"
  chmod 0640 "$config_path"
  echo "[CREATE] $config_path (copied from deploy/guardian.example.json)"
  echo "        EDIT $config_path before starting the guardian — paths + allowed_uids must match your environment."
fi

# 4. Build the guardian binary if not already built.
echo ""
echo "=== Building bootstrap-guardian ==="
(
  cd "$repo_root/bootstrap-guardian"
  cargo build --release --bin bootstrap-guardian --bin guardianctl
)
guardian_bin="$repo_root/bootstrap-guardian/target/release/bootstrap-guardian"
guardianctl_bin="$repo_root/bootstrap-guardian/target/release/guardianctl"
echo "[OK] built: $guardian_bin"
echo "[OK] built: $guardianctl_bin"

# 5. Render the launchd / systemd unit from the template.
echo ""
echo "=== Installing service unit ==="
os="$(uname -s)"
if [ "$os" = "Darwin" ]; then
  plist_src="$repo_root/bootstrap-guardian/deploy/com.trio.bootstrap-guardian.plist.tmpl"
  plist_dst="$HOME/Library/LaunchAgents/com.trio.bootstrap-guardian.plist"
  mkdir -p "$HOME/Library/LaunchAgents"
  sed \
    -e "s#TRIO_GUARDIAN_BIN#$guardian_bin#g" \
    -e "s#TRIO_GUARDIAN_CONFIG#$config_path#g" \
    -e "s#TRIO_RUN_DIR#$run_dir#g" \
    "$plist_src" > "$plist_dst"
  chmod 0644 "$plist_dst"
  echo "[CREATE] $plist_dst"
  echo ""
  echo "To load and start:"
  echo "  launchctl bootstrap gui/\$(id -u) $plist_dst"
  echo "  launchctl kickstart gui/\$(id -u)/com.trio.bootstrap-guardian"
  echo "To verify:"
  echo "  $guardianctl_bin --config $config_path status"
elif [ "$os" = "Linux" ]; then
  unit_src="$repo_root/bootstrap-guardian/deploy/bootstrap-guardian.service.tmpl"
  unit_dst="/etc/systemd/system/bootstrap-guardian.service"
  # UID separation is mandatory in prod — /review security flagged that
  # the guardian provides ZERO defense-in-depth when it runs as the same
  # UID as the harness (harness can then directly read guardian.key and
  # mint HMACs, or just bypass the socket entirely and fs::write to
  # protected paths since file permissions permit it).
  harness_user="${TRIO_HARNESS_USER:-$(id -un)}"
  guardian_user="${TRIO_GUARDIAN_USER:-$harness_user}"
  guardian_group="${TRIO_GUARDIAN_GROUP:-$(id -gn)}"

  if [ "$env_name" = "prod" ] && [ "$guardian_user" = "$harness_user" ] \
      && [ "${TRIO_ALLOW_SAME_UID:-0}" != "1" ]; then
    cat >&2 <<EOF
ERROR: guardian UID equals harness UID ($guardian_user).
  When they are the same user, the guardian provides no real protection —
  the harness can read guardian.key (mode 0400) and mint any HMAC, or
  just write to protected paths directly since the file permissions
  allow it.

  Set TRIO_GUARDIAN_USER to a dedicated system user (e.g. trio-guardian)
  that does NOT share a group with the harness user. The guardian-owned
  protected paths should be 0644 owned by \$TRIO_GUARDIAN_USER so the
  harness can read them but not write.

  If you understand the trade-off and are building a dev/sandbox
  deployment, set TRIO_ALLOW_SAME_UID=1 and re-run.
EOF
    exit 4
  fi
  if [ "$guardian_user" = "$harness_user" ]; then
    echo "[WARN] guardian UID == harness UID. Defense-in-depth disabled." >&2
    echo "[WARN] Running with TRIO_ALLOW_SAME_UID=1 override." >&2
  fi
  # allowed_roots aren't read from config here — operator edits the unit file afterwards.
  allowed_roots_placeholder="$run_dir"
  echo "Rendering unit (will need sudo to install)..."
  tmp_unit="$(mktemp)"
  sed \
    -e "s#TRIO_GUARDIAN_BIN#$guardian_bin#g" \
    -e "s#TRIO_GUARDIAN_CONFIG#$config_path#g" \
    -e "s#TRIO_RUN_DIR#$run_dir#g" \
    -e "s#TRIO_GUARDIAN_USER#$guardian_user#g" \
    -e "s#TRIO_GUARDIAN_GROUP#$guardian_group#g" \
    -e "s#TRIO_ALLOWED_ROOTS#$allowed_roots_placeholder#g" \
    "$unit_src" > "$tmp_unit"
  echo "[NEXT] sudo mv $tmp_unit $unit_dst && sudo systemctl daemon-reload"
  echo "[NEXT] sudo systemctl enable --now bootstrap-guardian"
  echo "[NEXT] $guardianctl_bin --config $config_path status"
else
  echo "Unsupported OS: $os" >&2
  exit 3
fi

# 6. Install pre-commit hook if this is a git worktree.
if [ -d "$repo_root/.git" ]; then
  hook="$repo_root/.git/hooks/pre-commit"
  src="$repo_root/hooks/pre-commit"
  if [ -f "$src" ]; then
    cp "$src" "$hook"
    chmod +x "$hook"
    echo "[OK] installed pre-commit hook from $src"
  fi
fi

echo ""
echo "=== Done. ==="
echo "Edit $config_path to match your environment, then start the guardian."
