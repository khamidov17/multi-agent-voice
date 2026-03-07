#!/bin/bash
# Claudir Monitor - checks if bot is running and healthy

CONFIG="${1:-claudir.json}"
LOG_DIR="data/prod/logs"
BOT_CMD="./target/release/claudir $CONFIG"

check_process() {
    pgrep -f "$BOT_CMD" > /dev/null
}

check_recent_activity() {
    # Check if log file was modified in last 5 minutes
    LOG_FILE="$LOG_DIR/claudir.log"
    if [ -f "$LOG_FILE" ]; then
        find "$LOG_FILE" -mmin -5 | grep -q .
        return $?
    fi
    return 1
}

start_bot() {
    echo "$(date): Starting claudir..."
    nohup $BOT_CMD >> /tmp/claudir-monitor.log 2>&1 &
    sleep 5
}

echo "Claudir Monitor started - watching: $CONFIG"
echo "Press Ctrl+C to stop"

while true; do
    if ! check_process; then
        echo "$(date): Bot not running! Restarting..."
        start_bot
    fi
    sleep 30
done
