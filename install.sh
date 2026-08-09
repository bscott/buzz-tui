#!/usr/bin/env sh
# Install buzztui, a terminal client for Buzz relays.
#
#   curl -fsSL https://raw.githubusercontent.com/bscott/buzz-tui/main/install.sh | sh
#
# Environment:
#   BUZZTUI_VERSION   tag to install, defaults to the latest release
#   BUZZTUI_BIN_DIR   where to put the binary, defaults to ~/.local/bin
#
# Written for POSIX sh so it runs under dash and busybox, not only bash.

set -eu

REPO="bscott/buzz-tui"
VERSION="${BUZZTUI_VERSION:-}"
BIN_DIR="${BUZZTUI_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "this installer needs $1"
}

# --------------------------------------------------------------- environment

need uname
need mkdir
need tar

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
    fetch_to() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    fetch_to() { wget -qO "$2" "$1"; }
else
    die "this installer needs curl or wget"
fi

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "unsupported operating system: $os. build from source instead: https://github.com/$REPO" ;;
esac

case "$arch" in
    x86_64 | amd64)  arch_part="x86_64" ;;
    aarch64 | arm64) arch_part="aarch64" ;;
    *) die "unsupported architecture: $arch. build from source instead: https://github.com/$REPO" ;;
esac

target="${arch_part}-${os_part}"

# ------------------------------------------------------------------- version

if [ -z "$VERSION" ]; then
    say "resolving the latest release..."
    # Ask the API for the release rather than following /latest, so a failure
    # is a clear error instead of an HTML page landing in the tarball.
    VERSION=$(
        fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
        | head -n1
    )
    [ -n "$VERSION" ] || die "could not determine the latest release; set BUZZTUI_VERSION"
fi

archive="buzztui-${VERSION}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$VERSION"

say "installing buzztui $VERSION for $target"

# ------------------------------------------------------------------ download

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t buzztui)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch_to "$base/$archive" "$tmp/$archive" \
    || die "no build for $target in $VERSION. see https://github.com/$REPO/releases"

# Verify against the release checksums when a hasher is available. A silent
# corrupt download is worse than a slow one, and this is a binary people run.
if fetch_to "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        hasher="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        hasher="shasum -a 256"
    else
        hasher=""
    fi

    if [ -n "$hasher" ]; then
        expected=$(sed -n "s/^\([0-9a-f]\{64\}\) *[* ]*$archive$/\1/p" "$tmp/SHA256SUMS" | head -n1)
        if [ -n "$expected" ]; then
            actual=$(cd "$tmp" && $hasher "$archive" | cut -d' ' -f1)
            [ "$expected" = "$actual" ] || die "checksum mismatch for $archive"
            say "checksum verified"
        else
            say "warning: $archive is not listed in SHA256SUMS; skipping verification"
        fi
    else
        say "warning: no sha256 tool found; skipping verification"
    fi
else
    say "warning: no SHA256SUMS published for $VERSION; skipping verification"
fi

# ------------------------------------------------------------------- install

tar -xzf "$tmp/$archive" -C "$tmp"
binary=$(find "$tmp" -type f -name buzztui -perm -u+x 2>/dev/null | head -n1)
[ -n "$binary" ] || die "the archive did not contain a buzztui binary"

mkdir -p "$BIN_DIR"
# Install to a temporary name and move it into place, so a running buzztui is
# never replaced underneath itself.
cp "$binary" "$BIN_DIR/.buzztui.new"
chmod 755 "$BIN_DIR/.buzztui.new"
mv "$BIN_DIR/.buzztui.new" "$BIN_DIR/buzztui"

say "installed $BIN_DIR/buzztui"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        say ""
        say "$BIN_DIR is not on your PATH. add it:"
        say "  export PATH=\"\$PATH:$BIN_DIR\""
        ;;
esac

say ""
say "run buzztui to choose a community and an identity."
