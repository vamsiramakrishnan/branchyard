#!/bin/sh
# Install Branchyard's verified prebuilt binaries and start guided setup.
#   curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
#   sh install.sh --version 0.0.1 --prefix "$HOME/.local" --no-setup
set -eu

repo=vamsiramakrishnan/branchyard
version=${BRANCHYARD_VERSION:-}
prefix=${BRANCHYARD_PREFIX:-${HOME:-}/.local}
base_url=
setup=auto
yes=0
dry_run=0
runtime=0

fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
usage() {
    cat <<'HELP'
Install Branchyard without a Rust toolchain or a source checkout.

Options:
  --version VERSION  install this release (default: latest stable release)
  --prefix DIR       install into DIR/bin (default: ~/.local)
  --base-url URL     use an HTTPS release mirror or a file:// directory
  --runtime          install the Linux GNU build with Microsandbox support
  --setup            start the guided setup after installation
  --no-setup         install binaries only
  --yes              take setup defaults without a terminal; creates no cloud resources
  --dry-run          show the release and installation path without downloading or writing
  -h, --help         print this help
HELP
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version|--prefix|--base-url)
            option=$1; [ "$#" -ge 2 ] || fail "$option needs a value"
            case "$option" in --version) version=$2 ;; --prefix) prefix=$2 ;; --base-url) base_url=$2 ;; esac
            shift 2 ;;
        --version=*) version=${1#*=}; shift ;;
        --prefix=*) prefix=${1#*=}; shift ;;
        --base-url=*) base_url=${1#*=}; shift ;;
        --runtime) runtime=1; shift ;;
        --setup) setup=yes; shift ;;
        --no-setup) setup=no; shift ;;
        --yes) yes=1; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option $1; run with --help" ;;
    esac
done
version=${version#v}
case "$version" in *[!0-9A-Za-z.+-]*) fail 'version must be a release version such as 0.0.1' ;; esac
[ -n "$prefix" ] && [ "$prefix" != / ] || fail 'choose a nonempty --prefix directory you own'
case "$prefix" in /*) ;; *) fail '--prefix must be an absolute path' ;; esac
case "$base_url" in
    http://*) fail "refusing plain http: $base_url" ;;
    https://*|file://*|'') ;;
    *://*) fail '--base-url must use https:// or file://' ;;
    *) base_url="file://$base_url" ;;
esac
if [ -z "$base_url" ]; then
    if [ -n "$version" ]; then base_url="https://github.com/$repo/releases/download/v$version"
    else base_url="https://github.com/$repo/releases/latest/download"; fi
fi
if [ "$dry_run" = 1 ]; then
    printf 'would install by, branchyard-server and branchyard-bridge from %s into %s/bin\n' "$base_url" "$prefix"
    [ "$setup" = no ] || printf 'would continue with guided setup when a terminal is available\n'
    exit 0
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/branchyard-install.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
fetch() {
    case "$base_url" in
        file://*) cp "${base_url#file://}/$1" "$work/$1" ;;
        https://*)
            if command -v curl >/dev/null 2>&1; then
                curl --proto '=https' --tlsv1.2 -fsSL "$base_url/$1" -o "$work/$1"
            elif command -v wget >/dev/null 2>&1; then
                wget --https-only -q "$base_url/$1" -O "$work/$1"
            else fail 'install curl or wget, then rerun this command'; fi ;;
    esac || fail "could not download $1 from $base_url; check the release and retry"
}

# dist owns platform selection, archive checksums, installation and shell PATH setup.
# Its sha256 implementation expects sha256sum; provide macOS's equivalent locally.
if ! command -v sha256sum >/dev/null 2>&1; then
    command -v shasum >/dev/null 2>&1 || fail 'install sha256sum or shasum to verify downloads'
    mkdir "$work/bin"
    cat > "$work/bin/sha256sum" <<'SHIM'
#!/bin/sh
exec shasum -a 256 "$@"
SHIM
    chmod 0755 "$work/bin/sha256sum"
    PATH="$work/bin:$PATH"; export PATH
fi
case "$base_url" in
    file://*)
        mirror=$(cd "${base_url#file://}" && pwd) || fail 'release mirror directory does not exist'
        base_url="file://$mirror" ;;
esac
packages="branchyard-cli branchyard-server branchyard-bridge"
first=branchyard-cli
if [ "$runtime" = 1 ]; then packages=branchyard-runtime; first=branchyard-runtime; fi
fetch "$first-installer.sh"
release_version=$(sed -n 's/^APP_VERSION="\([0-9A-Za-z.+-]*\)"$/\1/p' "$work/$first-installer.sh")
[ -n "$release_version" ] || fail 'release installer has no version; use a valid Branchyard release'
case "$release_version" in *[!0-9A-Za-z.+-]*) fail 'release installer has an invalid version' ;; esac
if [ -n "$version" ] && [ "$release_version" != "$version" ]; then
    fail "requested $version but mirror supplied $release_version"
fi
# Pin all remaining downloads to the same release even if latest changes midway.
case "$base_url" in
    "https://github.com/$repo/releases/latest/download")
        base_url="https://github.com/$repo/releases/download/v$release_version" ;;
esac
for package in $packages; do
    [ "$package" = "$first" ] || fetch "$package-installer.sh"
done
for package in $packages; do
    actual_version=$(sed -n 's/^APP_VERSION="\([0-9A-Za-z.+-]*\)"$/\1/p' "$work/$package-installer.sh")
    [ "$actual_version" = "$release_version" ] || fail "$package installer belongs to a different release"
done
export BRANCHYARD_PREFIX="$prefix"
export INSTALLER_DOWNLOAD_URL="$base_url"
export CARGO_DIST_FORCE_INSTALL_DIR="$prefix/bin"
export INSTALLER_NO_MODIFY_PATH=${INSTALLER_NO_MODIFY_PATH:-0}
for package in $packages; do
    sh "$work/$package-installer.sh" || fail "$package installation failed; rerun the same command to resume"
done
PATH="$prefix/bin:$PATH"; export PATH
for binary in by branchyard-server branchyard-bridge; do
    "$prefix/bin/$binary" --version >/dev/null ||
        fail "installed $binary could not execute; check native runtime libraries and rerun the installer"
done
installed_version=$("$prefix/bin/by" --version)
printf 'Installed %s into %s/bin\n' "$installed_version" "$prefix"
case ":${PATH#"$prefix/bin:"}:" in
    *":$prefix/bin:"*) ;;
    *) printf 'For future shells, add %s/bin to PATH.\n' "$prefix" ;;
esac
if [ "$setup" != no ]; then
    if [ "$yes" = 1 ]; then
        "$prefix/bin/by" setup --yes --up
    elif ( : </dev/tty ) 2>/dev/null; then
        "$prefix/bin/by" setup --guided </dev/tty
    elif [ "$setup" = yes ]; then
        fail "installed successfully; guided setup needs a terminal. Run $prefix/bin/by setup --guided, or rerun with --setup --yes"
    else
        printf 'Next: %s/bin/by setup --guided\n' "$prefix"
    fi
fi
