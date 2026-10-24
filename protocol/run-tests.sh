#!/bin/sh
# Run the protocol tests on the host.
#
# The firmware's .cargo/config.toml is found by walking *up from the current
# directory* and sets the ESP32 target plus build-std, which makes a host test
# run fail with "duplicate lang item in crate core". Running from a directory
# outside the tree avoids that, so this script does exactly that.
set -e

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cd "$tmp"
# format and lints run from the same vantage point, so a caller needs the tools
# installed but not the ESP target
if [ -n "$LINT" ]; then
    cargo fmt --manifest-path "$here/Cargo.toml" --check
    cargo clippy --manifest-path "$here/Cargo.toml" --all-targets -- -D warnings
fi
exec cargo test --manifest-path "$here/Cargo.toml" "$@"
