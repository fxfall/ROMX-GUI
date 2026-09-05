#!/usr/bin/env sh
set -eu

# Development-only helper.  Production builds consume the committed file in
# crates/libromx-sys/src/bindings.rs and therefore do not require bindgen.
repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
header=${ROMX_LIBROMX_DIR:-"$repo_dir/vendor/libromx"}/include/romx/romx.h
output="$repo_dir/crates/libromx-sys/src/bindings.rs"

if ! command -v bindgen >/dev/null 2>&1; then
    echo "bindgen is required only to regenerate bindings; install bindgen-cli 0.72.x" >&2
    exit 1
fi
bindgen "$header" \
    --clang-arg "-I$(dirname "$(dirname "$header")")" \
    --allowlist-type 'romx_.*' \
    --allowlist-function 'romx_.*' \
    --allowlist-var 'ROMX_VERSION_.*|ROMX_ERROR_MESSAGE_CAPACITY' \
    --output "$output"
