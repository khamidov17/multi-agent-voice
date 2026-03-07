#!/bin/bash
# Wait for new bug reports from the bot
# Exits when there's new feedback to review
# Usage: ./scripts/wait-for-feedback.sh [data_dir]

DATA_DIR="${1:-data/prod}"
FEEDBACK_FILE="$DATA_DIR/feedback.log"
STATE_FILE="$DATA_DIR/.feedback_offset"

# Initialize state file if it doesn't exist
if [ ! -f "$STATE_FILE" ]; then
    echo "0" > "$STATE_FILE"
fi

echo "Watching for bug reports in $FEEDBACK_FILE..."
echo "Press Ctrl+C to stop"

while true; do
    # Get current file size (0 if file doesn't exist)
    if [ -f "$FEEDBACK_FILE" ]; then
        CURRENT_SIZE=$(stat -c%s "$FEEDBACK_FILE" 2>/dev/null || stat -f%z "$FEEDBACK_FILE" 2>/dev/null)
    else
        CURRENT_SIZE=0
    fi

    # Get last seen offset
    LAST_OFFSET=$(cat "$STATE_FILE" 2>/dev/null || echo "0")

    # Check if there's new content
    if [ "$CURRENT_SIZE" -gt "$LAST_OFFSET" ]; then
        echo ""
        echo "=== NEW BUG REPORT(S) ==="
        echo ""

        # Read new content from the offset
        tail -c +$((LAST_OFFSET + 1)) "$FEEDBACK_FILE"

        # Update state
        echo "$CURRENT_SIZE" > "$STATE_FILE"

        echo ""
        echo "=== END OF NEW REPORTS ==="
        echo ""

        # Exit so Claude Code can process
        exit 0
    fi

    # No new content, wait and retry
    sleep 5
done
