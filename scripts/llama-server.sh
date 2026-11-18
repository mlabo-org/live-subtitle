#!/bin/bash
# Builds llama.cpp's llama-server (the built-in translation engine) from a pinned release and prints the path
# of the binary. The build is kept outside the source tree and reused until the pinned release changes.
#   scripts/llama-server.sh
#   LLAMA_SERVER_CACHE   where builds are kept; default ~/Library/Caches/LiveSubtitle
set -euo pipefail

# The llama.cpp release the app is tested with, and the commit that tag must point at.
TAG="b12503"
COMMIT="4577932fab595eaa8b27ad71734d5de1f3c07d8c"

# The folder is named after the release and this script, so a change of the build options below builds anew.
OPTIONS="$(shasum -a 256 "$0" | cut -c1-12)"
CACHE="${LLAMA_SERVER_CACHE:-$HOME/Library/Caches/LiveSubtitle}/llama.cpp-$TAG-$OPTIONS"
if [ -x "$CACHE/llama-server" ] && [ -f "$CACHE/LICENSE" ]; then
    echo "$CACHE/llama-server"
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
git clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$WORK/src" >&2
GOT="$(git -C "$WORK/src" rev-parse HEAD)"
if [ "$GOT" != "$COMMIT" ]; then
    echo "llama.cpp $TAG is at $GOT, expected $COMMIT" >&2
    exit 1
fi
# One self-contained binary: static libraries, and the Metal shaders embedded instead of a .metallib beside it.
# The browser UI is left out: the app talks to the server itself, and building the UI downloads its files.
# OpenSSL is left out too: the server only listens on 127.0.0.1 and never fetches anything, and linking it
# would tie the binary to the OpenSSL of the machine that built it.
cmake -S "$WORK/src" -B "$WORK/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 \
    -DBUILD_SHARED_LIBS=OFF \
    -DGGML_METAL=ON \
    -DGGML_METAL_EMBED_LIBRARY=ON \
    -DLLAMA_BUILD_UI=OFF \
    -DLLAMA_USE_PREBUILT_UI=OFF \
    -DLLAMA_OPENSSL=OFF \
    -DLLAMA_BUILD_TESTS=OFF \
    -DLLAMA_BUILD_EXAMPLES=OFF \
    -DLLAMA_BUILD_SERVER=ON >&2
cmake --build "$WORK/build" --target llama-server -j "$(sysctl -n hw.ncpu)" >&2
mkdir -p "$CACHE"
cp "$WORK/src/LICENSE" "$CACHE/LICENSE"
cp "$WORK/build/bin/llama-server" "$CACHE/llama-server"
echo "$CACHE/llama-server"
