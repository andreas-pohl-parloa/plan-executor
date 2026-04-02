#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$HOME/.plan-executor"
LOG_FILE="$BASE_DIR/daemon.log"
BINARY="$HOME/.cargo/bin/plan-executor"
MARKER="# plan-executor"

ACTION="${1:-install}"

# ── helpers ────────────────────────────────────────────────────────────────

_ensure_config() {
    local config="$BASE_DIR/config.json"
    if [[ ! -f "$config" ]]; then
        local workspace_dir
        workspace_dir="$(dirname "$SCRIPT_DIR")"
        mkdir -p "$BASE_DIR"
        cat > "$config" << EOCFG
{
  "watch_dirs": ["$workspace_dir", "$HOME/tools"],
  "plan_patterns": [".my/plans/*.md"],
  "auto_execute": false
}
EOCFG
        echo "Created config: $config"
        echo "  watch_dirs: $workspace_dir, ~/tools"
    fi
}

_remove_legacy_launchd() {
    local label="com.plan-executor.daemon"
    local plist="$HOME/Library/LaunchAgents/$label.plist"
    if launchctl list "$label" &>/dev/null; then
        echo "Removing legacy launchd agent..."
        launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || \
            launchctl unload "$plist" 2>/dev/null || true
    fi
    if [[ -f "$plist" ]]; then rm -f "$plist" && echo "Removed legacy plist."; fi
}

_add_shell_hook() {
    local hook='command -v plan-executor >/dev/null 2>&1 && plan-executor ensure 2>/dev/null'
    for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile"; do
        [[ -f "$rc" ]] || continue
        if grep -qF "plan-executor ensure" "$rc" 2>/dev/null; then
            return  # already present
        fi
    done
    # Add to the detected shell rc
    local rc
    case "$(basename "${SHELL:-zsh}")" in
        zsh)  rc="$HOME/.zshrc" ;;
        bash) rc="${HOME}/.bash_profile"; [[ -f "$HOME/.bashrc" ]] && rc="$HOME/.bashrc" ;;
        *)    rc="$HOME/.profile" ;;
    esac
    echo "" >> "$rc"
    echo "$MARKER" >> "$rc"
    echo "$hook" >> "$rc"
    echo "Added auto-start hook to $rc"
}

_remove_shell_hook() {
    for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
        [[ -f "$rc" ]] || continue
        if grep -qF "plan-executor" "$rc" 2>/dev/null; then
            sed -i.bak \
                -e "/^$MARKER$/d" \
                -e '/plan-executor ensure/d' \
                "$rc"
            rm -f "${rc}.bak"
            echo "Removed hook from $rc"
        fi
    done
}

case "$ACTION" in

# ── install ────────────────────────────────────────────────────────────────
install)
    _remove_legacy_launchd

    # Stop the running daemon (if any) via the binary itself.
    "$BINARY" stop 2>/dev/null || true

    echo "Updating git submodules..."
    git -C "$SCRIPT_DIR" submodule update --init --remote stream-json-view
    echo "Building and installing plan-executor..."
    cargo install --path "$SCRIPT_DIR"
    echo "Installed: $BINARY"

    # Install notification icon
    cp "$SCRIPT_DIR/assets/icon.png" "$BASE_DIR/icon.png"

    _ensure_config
    _add_shell_hook

    echo "Starting daemon..."
    "$BINARY" daemon

    echo ""
    echo "Done. Daemon is running. It will auto-start in new shell sessions."
    echo ""
    echo "  Logs:      tail -f $LOG_FILE"
    echo "  Stop:      $0 stop"
    echo "  Start:     $0 start"
    echo "  Restart:   $0 restart"
    echo "  Uninstall: $0 uninstall"
    ;;

# ── stop ──────────────────────────────────────────────────────────────────
stop)
    "$BINARY" stop
    ;;

# ── start ─────────────────────────────────────────────────────────────────
start)
    "$BINARY" daemon
    echo "Daemon started."
    ;;

# ── restart ───────────────────────────────────────────────────────────────
restart)
    "$BINARY" stop 2>/dev/null || true
    echo "Updating git submodules..."
    git -C "$SCRIPT_DIR" submodule update --init --remote stream-json-view
    echo "Building and installing plan-executor..."
    cargo install --path "$SCRIPT_DIR"
    echo "Installed: $BINARY"
    cp "$SCRIPT_DIR/assets/icon.png" "$BASE_DIR/icon.png"
    "$BINARY" daemon
    echo "Daemon restarted."
    ;;

# ── uninstall ──────────────────────────────────────────────────────────────
uninstall)
    "$BINARY" stop 2>/dev/null || true
    _remove_legacy_launchd
    _remove_shell_hook
    [[ -f "$BINARY" ]] && rm -f "$BINARY" && echo "Removed: $BINARY"
    echo ""
    echo "Done. Data directory $BASE_DIR was left intact."
    echo "Remove manually if no longer needed: rm -rf $BASE_DIR"
    ;;

*)
    echo "Usage: $0 [install|start|stop|restart|uninstall]" >&2
    exit 1
    ;;

esac
