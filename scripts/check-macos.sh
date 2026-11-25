#!/usr/bin/env bash
# Type-check (no link) the workspace for macOS from Linux: catches cfg(macos)
# compile errors without a Mac. Needs clang and `rustup target add
# aarch64-apple-darwin`. ring's C build wants a few SDK headers; tiny stubs
# stand in for them (check never links, so nothing they declare is used).
set -euo pipefail
cd "$(dirname "$0")/.."
stub="target/macsdk-stub"
mkdir -p "$stub"
printf '#pragma once\n#define TARGET_OS_MAC 1\n#define TARGET_OS_OSX 1\n#define TARGET_OS_IPHONE 0\n#define TARGET_OS_IOS 0\n#define TARGET_OS_SIMULATOR 0\n#define TARGET_OS_EMBEDDED 0\n' >"$stub/TargetConditionals.h"
printf '#pragma once\n#define assert(x) ((void)0)\n' >"$stub/assert.h"
printf '#pragma once\n#include <stddef.h>\nvoid *memcpy(void*,const void*,size_t);\nvoid *memset(void*,int,size_t);\nvoid *memmove(void*,const void*,size_t);\nint memcmp(const void*,const void*,size_t);\n' >"$stub/string.h"
export CC_aarch64_apple_darwin=clang
export CFLAGS_aarch64_apple_darwin="--target=arm64-apple-macos11 -ffreestanding -nostdlibinc -I$PWD/$stub"
exec cargo ${CARGO_CMD:-check} --target aarch64-apple-darwin --workspace --all-targets "$@"
