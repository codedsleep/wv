#!/bin/sh
# weave installer — downloads a prebuilt wv binary and puts it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/codedsleep/wv/main/install.sh | sh
#
# Options (pass after `-s --` when piping: `... | sh -s -- --dir ~/.local/bin`):
#   --dir DIR        install into DIR instead of the default
#   --version TAG    install a specific release (default: the latest)
#   -y, --yes        never prompt; take the default answer
#   --uninstall      remove an installed wv
#   -h, --help       print this and exit
#
# No Rust toolchain needed: the binaries are statically linked against musl and
# run on any Linux distribution.

set -eu

OWNER_REPO="codedsleep/wv"
BIN_NAME="wv"
VERSION="latest"
INSTALL_DIR=""
ASSUME_YES=0
UNINSTALL=0
WORKDIR=""

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    B=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RED=$(printf '\033[31m')
    YLW=$(printf '\033[33m'); GRN=$(printf '\033[32m'); R=$(printf '\033[0m')
else
    B=''; DIM=''; RED=''; YLW=''; GRN=''; R=''
fi

say()  { printf '%s==>%s %s\n' "$B" "$R" "$1"; }
warn() { printf '%swarning:%s %s\n' "$YLW" "$R" "$1" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$R" "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

cleanup() { [ -n "$WORKDIR" ] && rm -rf "$WORKDIR"; return 0; }
trap cleanup EXIT INT TERM

usage() { sed -n '/^# weave installer/,/^$/p' "$0" | sed 's/^#\{1,\} \{0,1\}//'; }

# Ask a yes/no question on the terminal rather than stdin, so this still works
# when the script itself arrived on stdin through a pipe.
confirm() {
    [ "$ASSUME_YES" -eq 1 ] && return 0
    if [ -r /dev/tty ]; then
        printf '%s [y/N] ' "$1" > /dev/tty
        read -r reply < /dev/tty || reply=""
    else
        return 1
    fi
    case "$reply" in [Yy]|[Yy][Ee][Ss]) return 0 ;; *) return 1 ;; esac
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dir)     [ $# -ge 2 ] || die "--dir needs a directory"; INSTALL_DIR="$2"; shift 2 ;;
        --dir=*)   INSTALL_DIR="${1#--dir=}"; shift ;;
        --version) [ $# -ge 2 ] || die "--version needs a tag"; VERSION="$2"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; shift ;;
        -y|--yes)  ASSUME_YES=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# ------------------------------------------------------------------ fetch ---

if have curl; then
    dl() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif have wget; then
    dl() { wget -qO "$2" "$1"; }
else
    die "need curl or wget to download."
fi

# ------------------------------------------------------------- uninstall ----

if [ "$UNINSTALL" -eq 1 ]; then
    found=""
    for d in ${INSTALL_DIR:-/usr/local/bin "$HOME/.local/bin" "$HOME/bin"}; do
        [ -f "$d/$BIN_NAME" ] && found="$found $d/$BIN_NAME"
    done
    [ -n "$found" ] || die "no $BIN_NAME found to uninstall."
    for f in $found; do
        if [ -w "$(dirname "$f")" ]; then rm -f "$f"; else sudo rm -f "$f"; fi
        say "removed $f"
    done
    exit 0
fi

# --------------------------------------------------------------- platform ---

[ "$(uname -s)" = "Linux" ] || die "weave is Linux only; this is $(uname -s)."

case "$(uname -m)" in
    x86_64|amd64)  TARGET="x86_64-unknown-linux-musl" ;;
    aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
    *) die "no prebuilt binary for $(uname -m). Build from source:
    cargo install --git https://github.com/$OWNER_REPO --locked" ;;
esac

ASSET="$BIN_NAME-$TARGET.tar.gz"
if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$OWNER_REPO/releases/latest/download"
else
    BASE="https://github.com/$OWNER_REPO/releases/download/$VERSION"
fi

# ----------------------------------------------------------- install path ---

# /usr/local/bin is already on everyone's PATH but needs root; ~/.local/bin
# needs no root but is not always on PATH. Prefer the one we can actually use.
if [ -z "$INSTALL_DIR" ]; then
    if [ -w /usr/local/bin ] || [ "$(id -u)" -eq 0 ] || have sudo; then
        INSTALL_DIR=/usr/local/bin
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi

SUDO=""
if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || SUDO="sudo"
fi
if [ -z "$SUDO" ] && [ ! -w "$INSTALL_DIR" ]; then
    SUDO="sudo"
fi
if [ -n "$SUDO" ] && ! have sudo; then
    die "$INSTALL_DIR needs root and sudo is not available. Try --dir ~/.local/bin"
fi

# -------------------------------------------------------------- download ----

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/weave-install.XXXXXX")

say "Downloading $ASSET ($VERSION)"
dl "$BASE/$ASSET" "$WORKDIR/$ASSET" || die "no release asset $ASSET.
Check https://github.com/$OWNER_REPO/releases for available versions."

# Checksums are published as one SHA256SUMS covering every asset.
if dl "$BASE/SHA256SUMS" "$WORKDIR/SHA256SUMS" 2>/dev/null; then
    if have sha256sum; then
        want=$(awk -v a="$ASSET" '$2 == a || $2 == "*"a {print $1}' "$WORKDIR/SHA256SUMS" | head -1)
        got=$(sha256sum "$WORKDIR/$ASSET" | awk '{print $1}')
        if [ -z "$want" ]; then
            warn "SHA256SUMS has no entry for $ASSET; skipping verification."
        elif [ "$want" != "$got" ]; then
            die "checksum mismatch for $ASSET.
  expected $want
  got      $got"
        else
            say "Checksum verified"
        fi
    else
        warn "sha256sum not found; skipping checksum verification."
    fi
else
    warn "no SHA256SUMS published for this release; skipping verification."
fi

tar -xzf "$WORKDIR/$ASSET" -C "$WORKDIR"
built=$(find "$WORKDIR" -type f -name "$BIN_NAME" -perm -u+x | head -1)
[ -n "$built" ] || die "the archive did not contain a $BIN_NAME binary."

# --------------------------------------------------------------- install ----

if [ -e "$INSTALL_DIR/$BIN_NAME" ]; then
    current=$("$INSTALL_DIR/$BIN_NAME" --version 2>/dev/null || echo "unknown version")
    confirm "$INSTALL_DIR/$BIN_NAME already exists ($current). Overwrite?" \
        || die "left the existing $BIN_NAME alone."
fi

if [ -n "$SUDO" ]; then
    say "Installing to $INSTALL_DIR ${DIM}(needs sudo)${R}"
    sudo mkdir -p "$INSTALL_DIR"
    sudo install -m 755 "$built" "$INSTALL_DIR/$BIN_NAME"
else
    say "Installing to $INSTALL_DIR"
    install -m 755 "$built" "$INSTALL_DIR/$BIN_NAME"
fi

# ------------------------------------------------------------------ PATH ----

on_path=0
case ":$PATH:" in *":$INSTALL_DIR:"*) on_path=1 ;; esac

printf '\n%s%s installed to %s/%s%s\n' "$GRN" "$BIN_NAME" "$INSTALL_DIR" "$BIN_NAME" "$R"
"$INSTALL_DIR/$BIN_NAME" --version 2>/dev/null || true

if [ "$on_path" -eq 0 ]; then
    printf '\n'
    warn "$INSTALL_DIR is not on your PATH. Add it:"
    case "$(basename "${SHELL:-sh}")" in
        fish) printf '\n    fish_add_path %s\n' "$INSTALL_DIR" ;;
        zsh)  printf '\n    echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc\n' "$INSTALL_DIR" ;;
        *)    printf '\n    echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$INSTALL_DIR" ;;
    esac
    printf '\nthen open a new shell and run %s%s%s.\n' "$B" "$BIN_NAME" "$R"
else
    printf 'Run %s%s%s to start a session.\n' "$B" "$BIN_NAME" "$R"
fi
