#!/bin/sh
# Installs azazel for the current user and puts it on PATH, or removes it again.
#
#   curl -fsSL https://raw.githubusercontent.com/sinnafuls/azazel-cli/main/install/install.sh | sh
#   ./install/install.sh                    from a source checkout (builds with cargo)
#   ./install/install.sh --from ./azazel    install a given binary
#   ./install/install.sh --uninstall        remove it (add --purge to also delete ~/.azazel)
#
# The binary comes from --from; else from `azazel` next to this script (an unpacked release);
# else it is built from the source tree this script sits in; else the latest GitHub release for
# this platform is downloaded and its SHA-256 checked. It goes to --dir (default ~/.local/bin, or
# $AZAZEL_INSTALL_DIR). When that directory is not on PATH, a line marked "# added by the azazel
# installer" is added to ~/.profile, and to ~/.bashrc and ~/.zshrc when they exist; --uninstall
# removes those lines again. Settings, logins and sessions live in ~/.azazel.
set -eu

MARKER='# added by the azazel installer'
RELEASES="${AZAZEL_RELEASE_BASE:-https://github.com/sinnafuls/azazel-cli/releases/latest/download}"
from=''
dir="${AZAZEL_INSTALL_DIR:-}"
modify_path=1
uninstall=0
purge=0

usage() {
    cat <<'EOF'
usage: install.sh [--from <binary>] [--dir <directory>] [--no-modify-path]
       install.sh --uninstall [--dir <directory>] [--purge]
EOF
}

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --from) from="$2"; shift 2 ;;
        --from=*) from="${1#--from=}"; shift ;;
        --dir) dir="$2"; shift 2 ;;
        --dir=*) dir="${1#--dir=}"; shift ;;
        --no-modify-path) modify_path=0; shift ;;
        --uninstall) uninstall=1; shift ;;
        --purge) purge=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown option $1" ;;
    esac
done

# Piped into `sh`, there is no script file and nothing next to it to use.
script_dir=''
if [ -f "$0" ] && [ "$(basename "$0")" = "install.sh" ]; then
    script_dir=$(cd "$(dirname "$0")" && pwd)
fi
azazel_home="${AZAZEL_HOME:-$HOME/.azazel}"
# A copy of this script is kept here, so the install can be removed later.
share_dir="${XDG_DATA_HOME:-$HOME/.local/share}/azazel"
rc_files="$HOME/.profile $HOME/.bashrc $HOME/.zshrc"
# Uninstalling without --dir removes the directory the last install used.
if [ -z "$dir" ] && [ "$uninstall" -eq 1 ] && [ -f "$share_dir/install-dir" ]; then
    dir=$(cat "$share_dir/install-dir")
fi
dir="${dir:-$HOME/.local/bin}"

# Removes the marked PATH line from every shell startup file that has one.
remove_path_lines() {
    for rc in $rc_files; do
        if [ -f "$rc" ] && grep -qF "$MARKER" "$rc"; then
            tmp="$rc.azazel-tmp"
            grep -vF "$MARKER" "$rc" > "$tmp" || true
            cat "$tmp" > "$rc"
            rm -f "$tmp"
            say "removed the PATH line from $rc"
        fi
    done
}

on_path() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$1" -O "$2"
    else
        fail "neither curl nor wget is installed"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "no sha256sum or shasum to check the download with"
    fi
}

if [ "$uninstall" -eq 1 ]; then
    if [ -e "$dir/azazel" ]; then
        rm -f "$dir/azazel"
        say "removed $dir/azazel"
    fi
    remove_path_lines
    if [ -d "$share_dir" ]; then
        rm -rf "$share_dir"
        say "removed $share_dir"
    fi
    if [ -d "$azazel_home" ]; then
        if [ "$purge" -eq 1 ]; then
            rm -rf "$azazel_home"
            say "removed $azazel_home (settings, logins and sessions)"
        else
            say "kept $azazel_home (settings, logins and sessions); --purge removes it"
        fi
    fi
    say "azazel is uninstalled"
    exit 0
fi

# The script copy kept for uninstalling: this file, or the one inside a downloaded release.
installer_copy=''
if [ -n "$script_dir" ]; then installer_copy="$script_dir/install.sh"; fi
work=''
cleanup() { if [ -n "$work" ]; then rm -rf "$work"; fi; }
trap cleanup EXIT

if [ -z "$from" ] && [ -n "$script_dir" ] && [ -f "$script_dir/azazel" ]; then
    from="$script_dir/azazel"
elif [ -z "$from" ] && [ -n "$script_dir" ] && [ -f "$script_dir/../crates/azazel-cli/Cargo.toml" ]; then
    command -v cargo >/dev/null 2>&1 ||
        fail "cargo is not on PATH: install Rust from https://rustup.rs or pass --from <binary>"
    say "building azazel (cargo build --release -p azazel-cli)..."
    (cd "$script_dir/.." && cargo build --release -p azazel-cli)
    from="$script_dir/../target/release/azazel"
elif [ -z "$from" ]; then
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) fail "no release is published for $(uname -s); build from source instead" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        arm64|aarch64) arch=aarch64 ;;
        *) fail "no release is published for $(uname -m); build from source instead" ;;
    esac
    asset="azazel-$os-$arch.tar.gz"
    work=$(mktemp -d)
    say "downloading $asset..."
    fetch "$RELEASES/$asset" "$work/$asset" || fail "could not download $RELEASES/$asset"
    fetch "$RELEASES/$asset.sha256" "$work/$asset.sha256" || fail "could not download the checksum"
    expected=$(cut -d ' ' -f 1 < "$work/$asset.sha256")
    actual=$(sha256_of "$work/$asset")
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"
    tar -xzf "$work/$asset" -C "$work"
    from="$work/azazel-$os-$arch/azazel"
    installer_copy="$work/azazel-$os-$arch/install.sh"
fi
[ -f "$from" ] || fail "$from is not a file"

mkdir -p "$dir"
# Copy then rename, so a running azazel keeps its old file and nobody sees half a binary.
cp "$from" "$dir/azazel.installing"
chmod 755 "$dir/azazel.installing"
mv -f "$dir/azazel.installing" "$dir/azazel"
say "installed $dir/azazel"
mkdir -p "$share_dir"
if [ -n "$installer_copy" ] && [ -f "$installer_copy" ] && [ "$installer_copy" != "$share_dir/install.sh" ]; then
    cp "$installer_copy" "$share_dir/install.sh"
    chmod 755 "$share_dir/install.sh"
fi
printf '%s\n' "$dir" > "$share_dir/install-dir"

if [ "$modify_path" -eq 1 ] && ! on_path "$dir"; then
    line="export PATH=\"$dir:\$PATH\" $MARKER"
    for rc in $rc_files; do
        # ~/.profile is created if missing; the others only get the line when the shell uses them.
        if [ "$rc" = "$HOME/.profile" ] || [ -f "$rc" ]; then
            if ! grep -qF "$MARKER" "$rc" 2>/dev/null; then
                printf '\n%s\n' "$line" >> "$rc"
                say "added $dir to PATH in $rc"
            fi
        fi
    done
    say "open a new terminal, or run: export PATH=\"$dir:\$PATH\""
fi

say "$("$dir/azazel" --version) is ready: run \`azazel\` in a project directory to start"
if [ -f "$share_dir/install.sh" ]; then
    say "update with \`azazel update\`; remove with: $share_dir/install.sh --uninstall"
fi
