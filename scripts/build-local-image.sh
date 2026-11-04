#!/bin/sh
# Build Redlib locally and wrap the binary in a small Alpine image.
#
# The host build reuses cargo's incremental cache: the first run compiles every
# dependency, later runs only what changed. Creating the image is then a COPY.
#
# Only `ring` (rustls' crypto backend) needs a C compiler; `musl-gcc` from
# musl-tools is enough on an aarch64 host because the build is native.
set -eu

IMAGE="${IMAGE:-redlib:local}"
TARGET=aarch64-unknown-linux-musl

cd "$(dirname "$0")/.."

CC_aarch64_unknown_linux_musl="${CC_aarch64_unknown_linux_musl:-musl-gcc}" \
AR_aarch64_unknown_linux_musl="${AR_aarch64_unknown_linux_musl:-ar}" \
"${CARGO:-cargo}" build --release --locked --target "$TARGET" "$@"

docker build -f Dockerfile.runtime -t "$IMAGE" .

echo "built $IMAGE"
