#!/bin/sh
# dist provides CARGO_DIST_TARGET and collects the three binaries from this directory.
set -eu
case "${CARGO_DIST_TARGET:-}" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu) ;;
    *) printf '%s\n' 'release_runtime.sh: native runtime requires a supported Linux GNU target' >&2; exit 1 ;;
esac
cargo build --manifest-path ../../Cargo.toml --locked --offline --profile dist \
    --target "$CARGO_DIST_TARGET" -p branchyard-cli -p branchyard-server -p branchyard-bridge \
    --features branchyard-cli/microsandbox,branchyard-cli/postgres,branchyard-server/postgres
for binary in by branchyard-server branchyard-bridge; do
    cp "../../target/$CARGO_DIST_TARGET/dist/$binary" "$binary"
done
