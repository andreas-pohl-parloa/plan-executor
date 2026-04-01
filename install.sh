#!/usr/bin/env bash
set -euo pipefail

# ── Debug logging ──────────────────────────────────────────────────────────
# Traces every command with a timestamp. Safe: no process substitution,
# no tee — stderr goes directly to the log file only.
# To watch live in another pane: tail -f /tmp/plan-executor-install.log
INSTALL_LOG="/tmp/plan-executor-install.log"
: > "$INSTALL_LOG"
exec 2>>"$INSTALL_LOG"
PS4='[$(date +%T.%3N)] + '
set -x
echo "=== install.sh started ===" >&2
# ──────────────────────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL="com.plan-executor.daemon"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
GUI_TARGET="gui/$(id -u)"
BASE_DIR="$HOME/.plan-executor"
LOG_FILE="$BASE_DIR/daemon.log"
BINARY="$HOME/.cargo/bin/plan-executor"

# Stop the daemon via its PID file only — no launchctl involved.
# Verifies the PID actually belongs to plan-executor before killing to avoid
# hitting a recycled PID that was reassigned to an unrelated process (e.g. WezTerm).
_stop_via_pid() {
    local pid
    pid=$(cat "$BASE_DIR/daemon.pid" 2>/dev/null | tr -d '[:space:]' || true)
    if [[ -z "$pid" ]]; then return; fi
    if ! kill -0 "$pid" 2>/dev/null; then return; fi

    # Confirm the running process is actually plan-executor before killing.
    local comm
    comm=$(ps -p "$pid" -o comm= 2>/dev/null || true)
    if [[ "$comm" != *"plan-executor"* ]]; then
        echo "PID $pid is not plan-executor (got: '$comm') — skipping kill, removing stale PID file."
        rm -f "$BASE_DIR/daemon.pid"
        return
    fi

    kill "$pid" 2>/dev/null || true
    echo "Stopped daemon (pid=$pid) — launchd will restart it automatically."
}

ACTION="${1:-install}"

case "$ACTION" in

# ── install ────────────────────────────────────────────────────────────────
install)
    # Stop the running daemon before replacing the binary so the file isn't
    # locked during install. launchd's KeepAlive brings it back on its own.
    _stop_via_pid

    echo "Building and installing plan-executor..."
    cargo install --path "$SCRIPT_DIR"
    echo "Installed: $BINARY"

    mkdir -p "$BASE_DIR" "$HOME/Library/LaunchAgents"

    # Create a default config if none exists, seeded with the workspace parent
    # of the repo being installed from so the daemon watches the right dirs.
    CONFIG_FILE="$BASE_DIR/config.json"
    if [[ ! -f "$CONFIG_FILE" ]]; then
        WORKSPACE_DIR="$(dirname "$SCRIPT_DIR")"
        cat > "$CONFIG_FILE" << EOCFG
{
  "watch_dirs": ["$WORKSPACE_DIR", "$HOME/tools"],
  "plan_patterns": [".my/plans/*.md"],
  "auto_execute": false
}
EOCFG
        echo "Created default config: $CONFIG_FILE"
        echo "  watch_dirs: $WORKSPACE_DIR, ~/tools"
        echo "  Edit $CONFIG_FILE to add or remove watched directories."
    fi

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

    # Register with launchd only on first install. On subsequent installs the
    # daemon is already registered and launchd restarts it automatically after
    # the PID-based stop above.
    if ! launchctl list "$LABEL" &>/dev/null; then
        launchctl bootstrap "$GUI_TARGET" "$PLIST"
        echo "Registered LaunchAgent."
    fi

    echo ""
    echo "Done. Waiting 10s to time when WezTerm dies (debugging)..."
    for i in 1 2 3 4 5 6 7 8 9 10; do
        echo "  $i/10 — still alive at $(date +%T)"
        sleep 1
    done
    echo "  survived 10s — WezTerm death is NOT during this script"
    echo ""
    echo "  Logs:      tail -f $LOG_FILE"
    echo "  Stop:      $0 stop"
    echo "  Start:     $0 start"
    echo "  Uninstall: $0 uninstall"
    ;;

# ── stop ──────────────────────────────────────────────────────────────────
stop)
    if launchctl list "$LABEL" &>/dev/null; then
        launchctl bootout "$GUI_TARGET/$LABEL" 2>/dev/null \
            || launchctl unload "$PLIST" 2>/dev/null \
            || true
        echo "Daemon stopped."
    else
        echo "Daemon is not running."
    fi
    ;;

# ── start ─────────────────────────────────────────────────────────────────
start)
    if launchctl list "$LABEL" &>/dev/null; then
        echo "Daemon is already running."
    else
        launchctl bootstrap "$GUI_TARGET" "$PLIST" \
            || launchctl load -w "$PLIST"
        echo "Daemon started."
    fi
    ;;

# ── uninstall ──────────────────────────────────────────────────────────────
uninstall)
    if launchctl list "$LABEL" &>/dev/null; then
        echo "Stopping daemon..."
        launchctl bootout "$GUI_TARGET/$LABEL" 2>/dev/null \
            || launchctl unload "$PLIST" 2>/dev/null \
            || true
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
    echo "Usage: $0 [install|start|stop|uninstall]" >&2
    exit 1
    ;;

esac
