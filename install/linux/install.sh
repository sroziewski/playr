#!/usr/bin/env bash
# Installs playr for the current user (no sudo) and adds it to "Open With" for media files.
#
#   install/linux/install.sh                  build with cargo and install
#   install/linux/install.sh --binary PATH    install a ready playr binary instead
#   install/linux/install.sh --install-deps   first apt-get the packages the build needs (sudo)
#   install/linux/install.sh --uninstall      remove everything an install added
#
# What it adds:
#   $PLAYR_BIN_DIR/playr                         (default ~/.local/bin)
#   $XDG_DATA_HOME/applications/playr.desktop    (default ~/.local/share)
#   $XDG_DATA_HOME/icons/hicolor/{256x256,scalable}/apps/playr.{png,svg}
#
# The desktop entry lists the media types, so file managers offer playr under right-click >
# Open With. It does not make playr the default; choose that in the file's Properties >
# Open With (or "Always use for this file type" in the chooser).
# Running it again updates the installation in place.
set -euo pipefail

# Must equal MEDIA_EXTENSIONS in src/ui.rs (tests/installers.rs checks it). Each extension
# maps to the MIME types the shared-mime-info database gives that kind of file; desktops match
# "Open With" against those types, not against extensions.
EXTENSION_MIME_TYPES=(
    "mp4:video/mp4"
    "m4v:video/x-m4v"
    "mov:video/quicktime"
    "mkv:video/x-matroska"
    "webm:video/webm"
    "avi:video/x-msvideo video/avi video/vnd.avi"
    "mp3:audio/mpeg audio/mp3"
    "flac:audio/flac audio/x-flac"
    "aac:audio/aac audio/x-aac"
    "m4a:audio/mp4 audio/x-m4a"
    "ogg:audio/ogg audio/x-vorbis+ogg audio/x-opus+ogg audio/x-flac+ogg video/ogg"
    "oga:audio/ogg"
    "wav:audio/x-wav audio/wav audio/vnd.wave"
    "m3u:audio/x-mpegurl audio/mpegurl"
    "m3u8:application/vnd.apple.mpegurl application/x-mpegurl"
    "pls:audio/x-scpls"
)

APT_PACKAGES=(build-essential pkg-config libasound2-dev)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd -- "$SCRIPT_DIR/../.." && pwd)
ASSETS_DIR="$REPO_DIR/assets"

BIN_DIR=${PLAYR_BIN_DIR:-"$HOME/.local/bin"}
DATA_DIR=${XDG_DATA_HOME:-"$HOME/.local/share"}
APPS_DIR="$DATA_DIR/applications"
ICONS_DIR="$DATA_DIR/icons/hicolor"
DESKTOP_FILE="$APPS_DIR/playr.desktop"
ICON_PNG="$ICONS_DIR/256x256/apps/playr.png"
ICON_SVG="$ICONS_DIR/scalable/apps/playr.svg"

die() { echo "install.sh: $*" >&2; exit 1; }

usage() {
    sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

mime_types() {
    local entry types=()
    for entry in "${EXTENSION_MIME_TYPES[@]}"; do
        # shellcheck disable=SC2206  # the types are space-separated on purpose
        types+=(${entry#*:})
    done
    # Unique, in first-seen order, each followed by ';' as the desktop entry spec wants.
    printf '%s\n' "${types[@]}" | awk '!seen[$0]++' | tr '\n' ';'
}

# Quotes a path for the Exec key: inside double quotes, the spec reserves " ` $ and \.
exec_quote() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//\`/\\\`}
    s=${s//\$/\\\$}
    # The desktop file is itself unescaped once more, so a backslash must be doubled again.
    s=${s//\\/\\\\}
    # % starts a field code (%f) anywhere in Exec.
    s=${s//%/%%}
    printf '"%s"' "$s"
}

missing_packages() {
    local missing=() pkg
    command -v dpkg-query >/dev/null 2>&1 || return 0
    for pkg in "${APT_PACKAGES[@]}"; do
        if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q 'install ok installed'; then
            missing+=("$pkg")
        fi
    done
    echo "${missing[@]+"${missing[@]}"}"
}

build() {
    local install_deps=$1 missing
    [[ -f "$REPO_DIR/Cargo.toml" ]] || die "no playr source in $REPO_DIR; pass --binary PATH"
    missing=$(missing_packages)
    if [[ -n $missing ]]; then
        if [[ $install_deps == yes ]]; then
            # build() prints only the binary path on stdout (the caller captures it), so
            # progress and apt's output go to stderr.
            echo "Installing build packages: $missing" >&2
            # shellcheck disable=SC2086
            sudo apt-get install -y $missing >&2 || die "apt-get install failed"
        else
            die "the build needs these packages: $missing
  install them with:  sudo apt-get install $missing
  or run this script with --install-deps"
        fi
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        [[ -x "$HOME/.cargo/bin/cargo" ]] && PATH="$HOME/.cargo/bin:$PATH"
    fi
    command -v cargo >/dev/null 2>&1 ||
        die "cargo was not found; install Rust from https://rustup.rs, or pass --binary PATH"
    echo "Building playr (cargo build --release)..." >&2
    # build() runs inside $(...), where set -e does not apply: every failure needs its own die,
    # or a failed build would go on to install the binary left by an earlier one.
    (cd "$REPO_DIR" && cargo build --release >&2) || die "cargo build failed"
    echo "$REPO_DIR/target/release/playr"
}

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$APPS_DIR" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -d $ICONS_DIR ]]; then
        gtk-update-icon-cache -q -f -t "$ICONS_DIR" 2>/dev/null || true
    fi
}

do_install() {
    local binary=$1 install_deps=$2
    if [[ -z $binary ]]; then
        binary=$(build "$install_deps")
    fi
    [[ -f $binary ]] || die "no such file: $binary"
    [[ -f "$ASSETS_DIR/playr.png" && -f "$ASSETS_DIR/playr.svg" ]] || die "icons missing from $ASSETS_DIR"

    mkdir -p "$BIN_DIR" "$APPS_DIR" "$(dirname "$ICON_PNG")" "$(dirname "$ICON_SVG")"
    # Copy to a temporary name and rename: replacing a running binary in place would fail
    # with "Text file busy", while a rename leaves the running copy intact.
    install -m 755 "$binary" "$BIN_DIR/.playr.new"
    mv -f "$BIN_DIR/.playr.new" "$BIN_DIR/playr"
    install -m 644 "$ASSETS_DIR/playr.png" "$ICON_PNG"
    install -m 644 "$ASSETS_DIR/playr.svg" "$ICON_SVG"

    cat >"$DESKTOP_FILE.new" <<EOF
[Desktop Entry]
Type=Application
Name=playr
GenericName=Media Player
Comment=Light, fast media player: video files, audio files and internet radio
Exec=$(exec_quote "$BIN_DIR/playr") %f
Icon=playr
Terminal=false
Categories=AudioVideo;Audio;Video;Player;
Keywords=video;audio;music;movie;radio;player;
MimeType=$(mime_types)
StartupWMClass=playr
EOF
    mv -f "$DESKTOP_FILE.new" "$DESKTOP_FILE"
    chmod 644 "$DESKTOP_FILE"

    refresh_caches
    echo "playr is installed: $BIN_DIR/playr"
    echo "Right-click a media file > Open With > playr."
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) echo "Note: $BIN_DIR is not on PATH, so 'playr' does not run from a terminal (the menu entry works)." ;;
    esac
}

do_uninstall() {
    rm -f "$BIN_DIR/playr" "$DESKTOP_FILE" "$ICON_PNG" "$ICON_SVG"
    refresh_caches
    echo "playr is uninstalled."
}

main() {
    local binary="" uninstall=no install_deps=no
    while [[ $# -gt 0 ]]; do
        case $1 in
            --binary) [[ $# -ge 2 ]] || die "--binary needs a path"; binary=$2; shift 2 ;;
            --binary=*) binary=${1#--binary=}; shift ;;
            --uninstall) uninstall=yes; shift ;;
            --install-deps) install_deps=yes; shift ;;
            -h | --help) usage; exit 0 ;;
            *) die "unknown option: $1 (see --help)" ;;
        esac
    done
    if [[ $uninstall == yes ]]; then do_uninstall; else do_install "$binary" "$install_deps"; fi
}

main "$@"
