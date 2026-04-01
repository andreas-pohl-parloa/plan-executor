#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL="com.plan-executor.daemon"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BASE_DIR="$HOME/.plan-executor"
LOG_FILE="$BASE_DIR/daemon.log"
BINARY="$HOME/.cargo/bin/plan-executor"

ACTION="${1:-install}"

case "$ACTION" in

# ── install ────────────────────────────────────────────────────────────────
install)
    echo "Building and installing plan-executor..."
    cargo install --path "$SCRIPT_DIR"
    echo "Installed: $BINARY"

    mkdir -p "$BASE_DIR" "$HOME/Library/LaunchAgents"

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
    echo "  Uninstall: $0 uninstall"
    ;;

# ── uninstall ──────────────────────────────────────────────────────────────
uninstall)
    if launchctl list "$LABEL" &>/dev/null; then
        echo "Stopping daemon..."
        launchctl unload "$PLIST" 2>/dev/null || true
    fi

    if [[ -f "$PLIST" ]]; then
        rm "$PLIST"
        echo "Removed: $PLIST"
    fi

    if [[ -f "$BINARY" ]]; then
        rm "$BINARY"
        echo "Removed: $BINARY"
    fi

    echo ""
    echo "Done. Data directory $BASE_DIR was left intact."
    echo "Remove it manually if no longer needed: rm -rf $BASE_DIR"
    ;;

*)
    echo "Usage: $0 [install|uninstall]" >&2
    exit 1
    ;;

esac
