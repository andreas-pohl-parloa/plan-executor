#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL="com.plan-executor.daemon"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BASE_DIR="$HOME/.plan-executor"
LOG_FILE="$BASE_DIR/daemon.log"
BINARY="$HOME/.cargo/bin/plan-executor"

# ── 1. Build and install ───────────────────────────────────────────────────

echo "Building and installing plan-executor..."
cargo install --path "$SCRIPT_DIR"
echo "Installed: $BINARY"

# ── 2. Create data directory ───────────────────────────────────────────────

mkdir -p "$BASE_DIR"

# ── 3. Write LaunchAgent plist ─────────────────────────────────────────────

mkdir -p "$HOME/Library/LaunchAgents"

cat > "$PLIST" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>

    <key>ProgramArguments</key>
    <array>
        <string>$BINARY</string>
        <string>daemon</string>
        <string>--foreground</string>
    </array>

    <!-- Start immediately and restart if it exits -->
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>

    <!-- Append stdout and stderr to the daemon log -->
    <key>StandardOutPath</key>
    <string>$LOG_FILE</string>
    <key>StandardErrorPath</key>
    <string>$LOG_FILE</string>
</dict>
</plist>
EOF

echo "Wrote LaunchAgent: $PLIST"

# ── 4. Load (or reload) the agent ─────────────────────────────────────────

# Unload first in case a previous version is running.
if launchctl list "$LABEL" &>/dev/null; then
    echo "Stopping existing daemon..."
    launchctl unload "$PLIST" 2>/dev/null || true
fi

launchctl load -w "$PLIST"

echo ""
echo "Done. plan-executor daemon is running and will start automatically at login."
echo ""
echo "  Logs:      tail -f $LOG_FILE"
echo "  Stop:      launchctl unload $PLIST"
echo "  Start:     launchctl load -w $PLIST"
echo "  Uninstall: launchctl unload $PLIST && rm $PLIST"
