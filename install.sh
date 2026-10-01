#!/bin/sh
# Install a released `by` and `branchyard-server` for this machine, after
# checking the archive's SHA-256 against the release's SHA256SUMS.
#
#   sh install.sh --version 0.1.0                  # into ~/.local/bin
#   sh install.sh --version 0.1.0 --prefix /opt/by
#   sh install.sh --version 0.1.0 --base-url file:///path/to/release-dir
#
# Download it, read it, then run it as the user who will own the files; it
# never calls sudo. For a prefix you cannot write, run it with the
# privileges that can (after reading it), not through a pipe.
# See docs/distribution.md.
set -eu

repo="vamsiramakrishnan/branchyard"
version="${BRANCHYARD_VERSION:-}"
prefix="${BRANCHYARD_PREFIX:-${HOME:-}/.local}"
base_url=""
target=""
dry_run=0

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    cat <<EOF
Options:
  --version V     the release to install (or BRANCHYARD_VERSION); required
  --prefix DIR    install into DIR/bin (default: ~/.local)
  --base-url URL  where the release's files are (default: its GitHub release);
                  https://, file:// or a directory
  --target T      the archive's target triple (default: this machine's)
  --dry-run       say what would be installed, change nothing
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; version=$2; shift 2 ;;
        --version=*) version=${1#*=}; shift ;;
        --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; prefix=$2; shift 2 ;;
        --prefix=*) prefix=${1#*=}; shift ;;
        --base-url) [ $# -ge 2 ] || die "--base-url needs a value"; base_url=$2; shift 2 ;;
        --base-url=*) base_url=${1#*=}; shift ;;
        --target) [ $# -ge 2 ] || die "--target needs a value"; target=$2; shift 2 ;;
        --target=*) target=${1#*=}; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
done

version=${version#v}
[ -n "$version" ] || die "name the release: --version X.Y.Z (or set BRANCHYARD_VERSION)"
case "$version" in
    *[!0-9A-Za-z.+-]*) die "unusable version $version" ;;
esac
[ -n "$prefix" ] || die "no --prefix and no HOME"

if [ -z "$target" ]; then
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64|amd64) arch=x86_64 ;;
        arm64|aarch64) arch=aarch64 ;;
        *) die "no prebuilt binary for $arch; build from source (cargo install --locked --path crates/branchyard-cli)" ;;
    esac
    case "$os" in
        Linux) target="$arch-unknown-linux-musl" ;;
        Darwin) target="$arch-apple-darwin" ;;
        *) die "no prebuilt binary for $os; build from source" ;;
    esac
fi

[ -n "$base_url" ] || base_url="https://github.com/$repo/releases/download/v$version"
name="branchyard-$version-$target"
archive="$name.tar.gz"

if [ "$dry_run" -eq 1 ]; then
    printf 'would install %s from %s into %s/bin\n' "$archive" "$base_url" "$prefix"
    exit 0
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/branchyard-install.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

fetch() {
    case "$base_url" in
        file://*) cp "${base_url#file://}/$1" "$work/$1" ;;
        http://*) die "refusing plain http: $base_url" ;;
        https://*)
            if command -v curl >/dev/null 2>&1; then
                curl --proto '=https' --tlsv1.2 -fsSL -o "$work/$1" "$base_url/$1"
            elif command -v wget >/dev/null 2>&1; then
                wget -q -O "$work/$1" "$base_url/$1"
            else
                die "needs curl or wget"
            fi ;;
        *) cp "$base_url/$1" "$work/$1" ;;
    esac || die "could not fetch $base_url/$1"
}

fetch SHA256SUMS
fetch "$archive"

expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$work/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS lists no $archive"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$work/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$work/$archive" | awk '{ print $1 }')
else
    die "needs sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive: expected $expected, got $actual; nothing was installed"

tar -xzf "$work/$archive" -C "$work"
[ -x "$work/$name/by" ] || die "$archive has no by"

mkdir -p "$prefix/bin" "$prefix/share/branchyard" 2>/dev/null \
    || die "cannot create $prefix/bin; choose a --prefix you own"
[ -w "$prefix/bin" ] || die "$prefix/bin is not writable; choose a --prefix you own"
for bin in by branchyard-server; do
    if [ -f "$work/$name/$bin" ]; then
        cp "$work/$name/$bin" "$prefix/bin/$bin.new"
        chmod 0755 "$prefix/bin/$bin.new"
        mv -f "$prefix/bin/$bin.new" "$prefix/bin/$bin"
    fi
done
cp "$work/$name/LICENSE" "$work/$name/THIRD_PARTY.md" "$prefix/share/branchyard/" 2>/dev/null || true
if [ -d "$work/$name/licenses" ]; then
    cp -R "$work/$name/licenses" "$prefix/share/branchyard/"
fi

printf 'installed %s into %s/bin (sha256 %s)\n' "$("$prefix/bin/by" --version 2>/dev/null || echo by)" "$prefix" "$actual"
case ":${PATH:-}:" in
    *":$prefix/bin:"*) ;;
    *) printf 'add %s/bin to your PATH\n' "$prefix" ;;
esac
