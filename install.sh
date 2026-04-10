#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_DIR="$HOME/.plan-executor"
LOG_FILE="$BASE_DIR/daemon.log"
MARKER="# plan-executor"
REPO_SLUG="andreas-pohl-parloa/plan-executor"
BINARY_NAME="plan-executor"

_get_install_dir() {
    if [[ -d "$HOME/bin" ]]; then
        echo "$HOME/bin"
    else
        mkdir -p "$HOME/.local/bin"
        echo "$HOME/.local/bin"
    fi
}

BINARY="$(_get_install_dir)/plan-executor"

ACTION="${1:-install}"

# ── helpers ────────────────────────────────────────────────────────────────

_detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64) echo "macos-arm64" ;;
                *)     echo "" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64)  echo "linux-x86_64" ;;
                aarch64) echo "linux-arm64" ;;
                *)       echo "" ;;
            esac
            ;;
        *) echo "" ;;
    esac
}

_install_from_binary() {
    local platform
    platform="$(_detect_platform)"
    if [[ -z "$platform" ]]; then
        return 1
    fi

    if ! command -v gh >/dev/null 2>&1; then
        echo "  gh CLI not found, skipping binary download."
        return 1
    fi

    local asset="plan-executor-${platform}.zip"
    local tmpdir
    tmpdir="$(mktemp -d)"

    # Download from the most recent release that has the matching asset
    # (skips releases where the build hasn't finished yet)
    echo "  Downloading pre-built binary ($platform)..."
    if ! gh release download \
            --repo "$REPO_SLUG" \
            --pattern "$asset" \
            --dir "$tmpdir" 2>/dev/null; then
        rm -rf "$tmpdir"
        echo "  Binary download failed."
        return 1
    fi

    if ! unzip -q "$tmpdir/$asset" -d "$tmpdir"; then
        rm -rf "$tmpdir"
        echo "  Unzip failed."
        return 1
    fi

    local extracted="$tmpdir/$BINARY_NAME"
    if [[ ! -f "$extracted" ]]; then
        rm -rf "$tmpdir"
        echo "  Binary not found in zip."
        return 1
    fi

    mkdir -p "$(dirname "$BINARY")"
    cp "$extracted" "$BINARY"
    chmod 755 "$BINARY"
    rm -rf "$tmpdir"

    # macOS: re-sign for Gatekeeper
    if [[ "$(uname -s)" = "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
        codesign --force --sign - "$BINARY" 2>/dev/null || true
    fi

    mkdir -p "$BASE_DIR"
    echo "binary" > "$BASE_DIR/install-mode"
    # Record installed version from the latest release tag
    local installed_tag
    installed_tag="$(gh release view --repo "$REPO_SLUG" --json tagName --jq '.tagName' 2>/dev/null || echo "unknown")"
    echo "${installed_tag#v}" > "$BASE_DIR/installed-version"
    return 0
}

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
    [[ "$(uname -s)" = "Darwin" ]] || return 0
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

    if [[ -z "${PE_SKIP_BINARY:-}" ]] && _install_from_binary; then
        echo "Installed from pre-built binary: $BINARY"
    else
        echo "Updating git submodules..."
        git -C "$SCRIPT_DIR" submodule update --init --remote stream-json-view
        echo "Building and installing plan-executor..."
        cargo install --path "$SCRIPT_DIR"
        echo "Installed: $BINARY"
        mkdir -p "$BASE_DIR"
        echo "source" > "$BASE_DIR/install-mode"
    fi

    # Install notification icon (only when running from repo checkout)
    if [[ -f "$SCRIPT_DIR/assets/icon.png" ]]; then
        cp "$SCRIPT_DIR/assets/icon.png" "$BASE_DIR/icon.png"
    fi

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
