#!/bin/sh
# install.sh — Dispatch CLI installer.
#
# When run via curl|sh, downloads the pre-built binary from GitHub Releases.
# When run from a repo checkout, builds from source (requires Rust/Cargo).
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/codesoda/dispatch-cli/main/install.sh | sh
#   ./install.sh [options]       # from a repo checkout (builds from source)
#
# Options:
#   --skip-symlink      Skip creating ~/.local/bin symlink
#   --help, -h          Show this help message
#
# Environment overrides:
#   DISPATCH_HOME       — Override ~/.dispatch install root
#   DISPATCH_LOCAL_BIN  — Override ~/.local/bin symlink directory
#   DISPATCH_REPO_OWNER — Override GitHub owner (default: codesoda)
#   DISPATCH_REPO_NAME  — Override GitHub repo  (default: dispatch-cli)
#   DISPATCH_REPO_REF   — Pin to a specific tag  (default: latest release)

set -eu

# --- Configuration (overridable for forks) ---

REPO_OWNER="${DISPATCH_REPO_OWNER:-codesoda}"
REPO_NAME="${DISPATCH_REPO_NAME:-dispatch-cli}"
REPO_REF="${DISPATCH_REPO_REF:-}"
BIN_NAME="dispatch"

# --- Color support ---

if [ "${NO_COLOR:-}" != "" ]; then
    USE_COLOR=0
elif [ -t 1 ] && command -v tput >/dev/null 2>&1 && [ "$(tput colors 2>/dev/null || echo 0)" -ge 8 ]; then
    USE_COLOR=1
else
    USE_COLOR=0
fi

if [ "$USE_COLOR" = 1 ]; then
    C_RESET='\033[0m'
    C_BOLD='\033[1m'
    C_DIM='\033[38;5;249m'
    C_OK='\033[38;5;114m'
    C_WARN='\033[38;5;216m'
    C_ERR='\033[38;5;210m'
    C_HEADER='\033[38;5;141m'
    C_CHECK='\033[38;5;151m'
else
    C_RESET=''
    C_BOLD=''
    C_DIM=''
    C_OK=''
    C_WARN=''
    C_ERR=''
    C_HEADER=''
    C_CHECK=''
fi

# --- Output helpers ---

header() {
    printf '\n%b%b%s%b\n' "$C_BOLD" "$C_HEADER" "$*" "$C_RESET"
    printf '%b%s%b\n' "$C_DIM" "$(echo "$*" | sed 's/./-/g')" "$C_RESET"
}

info() {
    printf '%b%s%b\n' "$C_OK" "$*" "$C_RESET"
}

dim() {
    printf '%b%s%b\n' "$C_DIM" "$*" "$C_RESET"
}

ok() {
    printf '%b✓ %s%b\n' "$C_CHECK" "$*" "$C_RESET"
}

ok_detail() {
    printf '%b✓ %s %b(%s)%b\n' "$C_CHECK" "$1" "$C_DIM" "$2" "$C_RESET"
}

warn() {
    printf '%b! %s%b\n' "$C_WARN" "$*" "$C_RESET" >&2
}

die() {
    printf '%b✗ %s%b\n' "$C_ERR" "$*" "$C_RESET" >&2
    exit 1
}

# --- Usage ---

usage() {
    cat <<'USAGE'
Dispatch CLI Installer

Usage:
  curl -sSf https://raw.githubusercontent.com/codesoda/dispatch-cli/main/install.sh | sh
  ./install.sh [options]

Options:
  --skip-symlink      Skip creating ~/.local/bin symlink
  --help, -h          Show this help message

Environment overrides:
  DISPATCH_HOME       — Override ~/.dispatch install root
  DISPATCH_LOCAL_BIN  — Override ~/.local/bin symlink directory
  DISPATCH_REPO_OWNER — Override GitHub owner (default: codesoda)
  DISPATCH_REPO_NAME  — Override GitHub repo  (default: dispatch-cli)
  DISPATCH_REPO_REF   — Pin to a specific tag  (default: latest release)
USAGE
}

# --- Argument parsing ---

SKIP_SYMLINK=0

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --skip-symlink)
                SKIP_SYMLINK=1
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                die "Unknown option: $1 (use --help)"
                ;;
        esac
        shift
    done
}

# --- Cleanup trap ---

TMP_DIR=""

cleanup() {
    if [ -n "$TMP_DIR" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}

trap cleanup EXIT INT TERM

# --- Global result variables ---

INSTALLED_BINARY=""
SOURCE_ROOT=""

# --- Detect architecture ---

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os_part="apple-darwin" ;;
        Linux)  os_part="unknown-linux-gnu" ;;
        *) die "Unsupported OS: $os — build from a clone instead." ;;
    esac

    case "$arch" in
        arm64|aarch64) arch_part="aarch64" ;;
        x86_64)        arch_part="x86_64" ;;
        *) die "Unsupported architecture: $arch — build from a clone instead." ;;
    esac

    echo "${arch_part}-${os_part}"
}

# --- Install from GitHub release ---

install_from_release() {
    if ! command -v curl >/dev/null 2>&1; then
        die "curl is required for remote install"
    fi

    target="$(detect_target)"

    header "Fetching latest release"

    if [ -n "$REPO_REF" ]; then
        tag="$REPO_REF"
    else
        # Get the latest release tag via redirect (avoids GitHub API rate limits)
        latest_url="https://github.com/$REPO_OWNER/$REPO_NAME/releases/latest"
        tag="$(curl -sSf -o /dev/null -w '%{redirect_url}' "$latest_url" | grep -oE '[^/]+$')"
        if [ -z "$tag" ]; then
            die "Could not determine latest release — check https://github.com/$REPO_OWNER/$REPO_NAME/releases"
        fi
    fi
    ok_detail "Release" "$tag"

    asset_name="${BIN_NAME}-${tag}-${target}.tar.gz"
    asset_url="https://github.com/$REPO_OWNER/$REPO_NAME/releases/download/${tag}/${asset_name}"

    TMP_DIR="$(mktemp -d)"
    info "Downloading $asset_name..."
    if ! curl -sSfL "$asset_url" -o "$TMP_DIR/$asset_name"; then
        die "Failed to download $asset_url"
    fi
    ok "Downloaded"

    tar xzf "$TMP_DIR/$asset_name" -C "$TMP_DIR"
    downloaded_binary="$TMP_DIR/${BIN_NAME}-${tag}-${target}/${BIN_NAME}"
    if [ ! -f "$downloaded_binary" ]; then
        die "Archive does not contain expected binary"
    fi

    install_binary "$downloaded_binary"
}

# --- Build from local source ---

build_from_source() {
    ok_detail "Source tree" "$SOURCE_ROOT"

    header "Checking prerequisites"
    if ! command -v cargo >/dev/null 2>&1; then
        die "cargo is required (install Rust: https://rustup.rs)"
    fi
    ok "cargo found"

    header "Building dispatch"
    if ! (cd "$SOURCE_ROOT" && cargo build --release); then
        die "cargo build failed"
    fi

    built_binary="$SOURCE_ROOT/target/release/$BIN_NAME"
    if [ ! -f "$built_binary" ]; then
        die "Build succeeded but binary not found at $built_binary"
    fi

    ok_detail "Built" "$built_binary"
    install_binary "$built_binary"
}

# --- Install binary to DISPATCH_HOME ---

install_binary() {
    src_binary="$1"
    dispatch_home="${DISPATCH_HOME:-$HOME/.dispatch}"
    bin_dir="$dispatch_home/bin"
    mkdir -p "$bin_dir"

    target_path="$bin_dir/$BIN_NAME"

    # Remove existing before install
    if [ -e "$target_path" ] || [ -L "$target_path" ]; then
        rm "$target_path"
    fi

    cp "$src_binary" "$target_path"
    chmod +x "$target_path"

    # macOS: remove quarantine and apply ad-hoc code signature
    secure_binary "$target_path"

    ok_detail "Installed" "$target_path"

    INSTALLED_BINARY="$target_path"
}

# --- macOS binary security ---

secure_binary() {
    case "$(uname -s)" in
        Darwin)
            if command -v xattr >/dev/null 2>&1; then
                xattr -dr com.apple.quarantine "$1" 2>/dev/null || true
                xattr -dr com.apple.provenance "$1" 2>/dev/null || true
            fi
            if command -v codesign >/dev/null 2>&1; then
                codesign --force --sign - "$1" 2>/dev/null || true
            fi
            ;;
    esac
}

# --- Symlink to ~/.local/bin ---

ensure_local_bin_symlink() {
    local_bin="${DISPATCH_LOCAL_BIN:-$HOME/.local/bin}"
    symlink_path="$local_bin/$BIN_NAME"

    if [ -e "$local_bin" ] && [ ! -d "$local_bin" ]; then
        warn "$local_bin exists but is not a directory — skipping symlink"
        return 1
    fi

    mkdir -p "$local_bin"

    if [ -L "$symlink_path" ]; then
        rm "$symlink_path"
    elif [ -e "$symlink_path" ]; then
        warn "$symlink_path exists and is not a symlink — skipping (remove it manually to fix)"
        return 1
    fi

    ln -s "$INSTALLED_BINARY" "$symlink_path"
    ok_detail "Symlinked" "$symlink_path -> $INSTALLED_BINARY"

    case ":${PATH}:" in
        *":${local_bin}:"*)
            ;;
        *)
            warn "$local_bin is not on your PATH — add it to your shell profile:"
            dim "  export PATH=\"$local_bin:\$PATH\""
            ;;
    esac

    return 0
}

# --- Summary ---

print_summary() {
    header "Done"

    ok_detail "Binary" "$INSTALLED_BINARY"

    printf '\n'
    dim "  Get started — paste this into your terminal or coding agent:"
    dim "  ─────────────────────────────────────────────────────────────"
    printf '\n'
    dim "    # 1. Initialise a project config"
    dim "    dispatch init"
    printf '\n'
    dim "    # 2. Start the broker (background it or use a second terminal)"
    dim "    dispatch serve &"
    printf '\n'
    dim "    # 3. Register a worker"
    dim "    dispatch register --name my-agent --role assistant \\"
    dim "      --description \"General-purpose coding agent\" \\"
    dim "      --capability code --capability review"
    printf '\n'
    dim "    # 4. Send a message to a worker (use the worker ID from step 3)"
    dim "    dispatch send --to <WORKER_ID> --body \"Hello from dispatch\""
    printf '\n'
    dim "    # 5. Listen for incoming messages"
    dim "    dispatch listen --worker-id <WORKER_ID>"
    printf '\n'
    dim "  Full docs: https://github.com/codesoda/dispatch-cli"
    dim "  Examples:   https://github.com/codesoda/dispatch-cli/tree/main/examples"
    printf '\n'
}

# --- Main ---

main() {
    parse_args "$@"

    printf '\n%b%bDispatch Installer%b\n' "$C_BOLD" "$C_HEADER" "$C_RESET"
    dim "━━━━━━━━━━━━━━━━━━"
    printf '\n'

    # If running from a repo checkout, build locally; otherwise grab the release binary
    script_dir="$(cd "$(dirname "$0")" && pwd)"
    if [ -f "$script_dir/Cargo.toml" ] && [ -d "$script_dir/src" ]; then
        SOURCE_ROOT="$script_dir"
        build_from_source
    else
        install_from_release
    fi

    if [ "$SKIP_SYMLINK" = 0 ]; then
        ensure_local_bin_symlink || true
    fi

    print_summary
}

main "$@"
