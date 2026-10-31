#!/usr/bin/env bash
# whisrs installer — downloads the latest prebuilt release tarball and runs setup.
#
# For end-user installs and updates. To build from a local checkout, see
# scripts/dev-install.sh; to build from source elsewhere, use
# `cargo install whisrs --locked` or the `whisrs-git` AUR package.
#
# Usage:
#   curl -sSf https://y0sif.github.io/whisrs/install.sh | bash
#   curl -sSf https://raw.githubusercontent.com/y0sif/whisrs/main/install.sh | bash
#   ./install.sh
#
# Environment:
#   WHISRS_VERSION=v0.1.11   Pin to a specific tag (default: latest)
#   WHISRS_MINIMAL=1         Use the minimal build (cloud-only, no whisper.cpp;
#                            still includes the tray icon, overlay, recordings and
#                            MCP server)

set -euo pipefail

# Colors.
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "  ${GREEN}${BOLD}$1${RESET} $2"; }
warn()  { echo -e "  ${YELLOW}$1${RESET}"; }
error() { echo -e "  ${RED}$1${RESET}"; }
step()  { echo -e "\n${BOLD}[$1/$TOTAL] $2${RESET}"; }

TOTAL=5

echo -e "\n${BOLD}whisrs installer${RESET} — voice-to-text dictation for Linux\n"

# ── Detect architecture ─────────────────────────────────────────────────

case "$(uname -m)" in
    x86_64)         ARCH="x86_64" ;;
    aarch64|arm64)  ARCH="aarch64" ;;
    *)
        error "Unsupported architecture: $(uname -m)"
        echo "  Prebuilt tarballs are published for x86_64 and aarch64 only."
        echo "  To build from source on this arch:"
        echo ""
        echo "    cargo install whisrs --locked"
        echo ""
        echo "  See https://github.com/y0sif/whisrs#installation for system deps."
        exit 1
        ;;
esac

# ── Step 1: Install runtime dependencies ────────────────────────────────

step 1 "Checking system dependencies..."

if command -v pacman &>/dev/null; then
    info "Detected:" "Arch Linux"
    needed=()
    for pkg in alsa-lib libxkbcommon ca-certificates curl tar; do
        if ! pacman -Qi "$pkg" &>/dev/null; then
            needed+=("$pkg")
        fi
    done
    if [ ${#needed[@]} -gt 0 ]; then
        echo "  Installing: ${needed[*]}"
        sudo pacman -S --needed --noconfirm "${needed[@]}"
    else
        echo "  All runtime libraries already installed."
    fi

elif command -v apt-get &>/dev/null; then
    info "Detected:" "Debian/Ubuntu"
    needed=()
    alsa_pkg="libasound2"
    if ! apt-cache policy "$alsa_pkg" 2>/dev/null | grep -q 'Candidate: [^(]'; then
        alsa_pkg="libasound2t64"
    fi
    for pkg in "$alsa_pkg" libxkbcommon0 ca-certificates curl tar; do
        if ! dpkg -s "$pkg" &>/dev/null 2>&1; then
            needed+=("$pkg")
        fi
    done
    if [ ${#needed[@]} -gt 0 ]; then
        echo "  Installing: ${needed[*]}"
        sudo apt-get update -qq
        sudo apt-get install -y -qq "${needed[@]}"
    else
        echo "  All runtime libraries already installed."
    fi

elif command -v dnf &>/dev/null; then
    info "Detected:" "Fedora/RHEL"
    needed=()
    for pkg in alsa-lib libxkbcommon ca-certificates curl tar; do
        if ! rpm -q "$pkg" &>/dev/null 2>&1; then
            needed+=("$pkg")
        fi
    done
    if [ ${#needed[@]} -gt 0 ]; then
        echo "  Installing: ${needed[*]}"
        sudo dnf install -y "${needed[@]}"
    else
        echo "  All runtime libraries already installed."
    fi

elif command -v zypper &>/dev/null; then
    info "Detected:" "openSUSE"
    sudo zypper install -y alsa libxkbcommon0 ca-certificates curl tar

else
    warn "Could not detect package manager."
    echo "  Please install manually: alsa-lib (or libasound2), libxkbcommon, curl, tar."
    echo "  Then re-run this script."
    exit 1
fi

# ── Step 2: Resolve the release tag ─────────────────────────────────────

step 2 "Resolving release tag..."

if [ -n "${WHISRS_VERSION:-}" ]; then
    TAG="$WHISRS_VERSION"
    info "Pinned:" "$TAG (WHISRS_VERSION)"
else
    # Follow the /releases/latest redirect to the canonical tag URL, then
    # extract the trailing tag name. No GitHub API token or jq required.
    TAG=$(curl -sSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/y0sif/whisrs/releases/latest" \
        | sed 's|.*/tag/||' | tr -d '[:space:]')

    if [ -z "$TAG" ] || [ "$TAG" = "latest" ]; then
        error "Could not resolve the latest release tag."
        echo "  Set WHISRS_VERSION=v0.1.11 (or similar) and re-run."
        exit 1
    fi
    info "Latest:" "$TAG"
fi

# ── Step 3: Download and extract the tarball ────────────────────────────

step 3 "Downloading prebuilt tarball..."

if [ "${WHISRS_MINIMAL:-}" = "1" ]; then
    ARTIFACT="whisrs-linux-${ARCH}-minimal.tar.gz"
    info "Variant:" "minimal (cloud backends only)"
else
    ARTIFACT="whisrs-linux-${ARCH}.tar.gz"
    info "Variant:" "full (with offline whisper.cpp)"
fi

URL="https://github.com/y0sif/whisrs/releases/download/${TAG}/${ARTIFACT}"
info "URL:" "$URL"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

if ! curl -fsSL -o "$TMPDIR/whisrs.tar.gz" "$URL"; then
    error "Download failed."
    echo "  Check that release ${TAG} has artifact ${ARTIFACT}:"
    echo "    https://github.com/y0sif/whisrs/releases/tag/${TAG}"
    exit 1
fi

tar xzf "$TMPDIR/whisrs.tar.gz" -C "$TMPDIR"

if [ ! -x "$TMPDIR/whisrs" ] || [ ! -x "$TMPDIR/whisrsd" ]; then
    error "Extracted tarball is missing the expected binaries."
    exit 1
fi

# ── Step 4: Install binaries and restart any running daemon ─────────────

step 4 "Installing binaries..."

sudo install -Dm755 "$TMPDIR/whisrs"  /usr/local/bin/whisrs
sudo install -Dm755 "$TMPDIR/whisrsd" /usr/local/bin/whisrsd
info "Installed:" "/usr/local/bin/whisrs"
info "Installed:" "/usr/local/bin/whisrsd"

# True when a whisrs user service is active under systemd or OpenRC.
service_is_active() {
    systemctl --user is-active whisrs.service &>/dev/null && return 0
    rc-service --user whisrs status &>/dev/null && return 0
    return 1
}

if service_is_active; then
    info "Restarting:" "running daemon (whisrs restart)"
    /usr/local/bin/whisrs restart || true
elif pgrep -x whisrsd &>/dev/null; then
    warn "whisrsd is running but not under a service manager."
    echo "  Restart it manually: pkill whisrsd; sleep 0.2; whisrsd &"
else
    echo "  No running daemon — it will start after setup."
fi

# ── Step 5: Run interactive setup ───────────────────────────────────────

step 5 "Running whisrs setup..."

echo ""
# Setup is a full-screen TUI and needs the terminal on stdin; under
# `curl | bash` stdin is the pipe, so it reads the terminal instead.
if [ -t 1 ] && (exec </dev/tty) 2>/dev/null; then
    /usr/local/bin/whisrs setup </dev/tty
else
    warn "No terminal to run setup in."
    echo "  Run it yourself: whisrs setup"
fi

if service_is_active; then
    /usr/local/bin/whisrs restart || true
    info "Daemon restarted" "with new config."
fi

echo -e "\n${GREEN}${BOLD}Installation complete!${RESET}\n"
