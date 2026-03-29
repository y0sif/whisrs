#!/usr/bin/env bash
# whisrs installer — builds from source, installs, and runs setup.
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/y0sif/whisrs/main/install.sh | bash
#   # or from a local clone:
#   ./install.sh

set -euo pipefail

# Colors.
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
BOLD='\033[1m'
DIM='\033[2m'
RESET='\033[0m'

info()  { echo -e "  ${GREEN}${BOLD}$1${RESET} $2"; }
warn()  { echo -e "  ${YELLOW}$1${RESET}"; }
error() { echo -e "  ${RED}$1${RESET}"; }
step()  { echo -e "\n${BOLD}[$1/$TOTAL] $2${RESET}"; }

TOTAL=5

echo -e "\n${BOLD}whisrs installer${RESET} — voice-to-text dictation for Linux\n"

# ── Step 1: Check / install system dependencies ─────────────────────────

step 1 "Checking system dependencies..."

# Detect package manager and distro.
install_deps() {
    if command -v pacman &>/dev/null; then
        info "Detected:" "Arch Linux"
        local needed=()
        for pkg in base-devel alsa-lib libxkbcommon clang cmake; do
            if ! pacman -Qi "$pkg" &>/dev/null; then
                needed+=("$pkg")
            fi
        done
        if [ ${#needed[@]} -gt 0 ]; then
            echo "  Installing: ${needed[*]}"
            sudo pacman -S --needed --noconfirm "${needed[@]}"
        else
            echo "  All system packages already installed."
        fi

    elif command -v apt-get &>/dev/null; then
        info "Detected:" "Debian/Ubuntu"
        local needed=()
        for pkg in build-essential libasound2-dev libxkbcommon-dev libclang-dev cmake; do
            if ! dpkg -s "$pkg" &>/dev/null 2>&1; then
                needed+=("$pkg")
            fi
        done
        if [ ${#needed[@]} -gt 0 ]; then
            echo "  Installing: ${needed[*]}"
            sudo apt-get update -qq
            sudo apt-get install -y -qq "${needed[@]}"
        else
            echo "  All system packages already installed."
        fi

    elif command -v dnf &>/dev/null; then
        info "Detected:" "Fedora/RHEL"
        local needed=()
        for pkg in gcc-c++ alsa-lib-devel libxkbcommon-devel clang-devel cmake; do
            if ! rpm -q "$pkg" &>/dev/null 2>&1; then
                needed+=("$pkg")
            fi
        done
        if [ ${#needed[@]} -gt 0 ]; then
            echo "  Installing: ${needed[*]}"
            sudo dnf install -y "${needed[@]}"
        else
            echo "  All system packages already installed."
        fi

    elif command -v zypper &>/dev/null; then
        info "Detected:" "openSUSE"
        sudo zypper install -y gcc-c++ alsa-devel libxkbcommon-devel clang cmake

    else
        warn "Could not detect package manager."
        echo "  Please install manually: C/C++ compiler, ALSA dev libs, libxkbcommon, clang, cmake"
        echo "  Then re-run this script."
        exit 1
    fi
}

install_deps

# Check for Rust toolchain.
if ! command -v cargo &>/dev/null; then
    warn "Rust toolchain not found. Installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    info "Rust installed:" "$(rustc --version)"
else
    info "Rust:" "$(rustc --version)"
fi

# ── Step 2: Clone or locate the source ──────────────────────────────────

step 2 "Building whisrs..."

# If we're already in a whisrs checkout, use it. Otherwise clone.
if [ -f "Cargo.toml" ] && grep -q 'name = "whisrs"' Cargo.toml 2>/dev/null; then
    SRCDIR="$(pwd)"
    info "Using local source:" "$SRCDIR"
else
    SRCDIR="$HOME/.cache/whisrs-src"
    if [ -d "$SRCDIR/.git" ]; then
        info "Updating:" "$SRCDIR"
        git -C "$SRCDIR" pull --ff-only
    else
        info "Cloning:" "whisrs → $SRCDIR"
        git clone https://github.com/y0sif/whisrs "$SRCDIR"
    fi
fi

# ── Step 3: Build and install ────────────────────────────────────────────

step 3 "Installing binaries..."

cargo install --path "$SRCDIR" --locked 2>&1 | tail -5

# Verify install.
if command -v whisrs &>/dev/null; then
    info "Installed:" "$(which whisrs)"
    info "Installed:" "$(which whisrsd)"
else
    # Might not be in PATH yet.
    if [ -f "$HOME/.cargo/bin/whisrs" ]; then
        warn "Binaries installed to ~/.cargo/bin/ but not in PATH."
        echo "  Add to your shell config:"
        echo "    fish:  fish_add_path ~/.cargo/bin"
        echo "    bash:  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        error "Build failed — check output above."
        exit 1
    fi
fi

# ── Step 4: Restart daemon if running ───────────────────────────────────

step 4 "Checking for running daemon..."

if systemctl --user is-active whisrs.service &>/dev/null; then
    info "Restarting:" "whisrs daemon (systemd service)"
    systemctl --user restart whisrs.service
    echo "  Daemon restarted with the new binary."
elif pgrep -x whisrsd &>/dev/null; then
    warn "whisrsd is running but not via systemd."
    echo "  Please restart it manually: kill \$(pgrep whisrsd) && whisrsd &"
else
    echo "  No running daemon found — it will start after setup."
fi

# ── Step 5: Run interactive setup ────────────────────────────────────────

step 5 "Running whisrs setup..."

echo ""
whisrs setup

# Restart daemon again if setup changed the config.
if systemctl --user is-active whisrs.service &>/dev/null; then
    systemctl --user restart whisrs.service
    info "Daemon restarted" "with new config."
fi

echo -e "\n${GREEN}${BOLD}Installation complete!${RESET}"
echo ""
