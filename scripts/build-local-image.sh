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

# The release profile in Cargo.toml enables fat LTO and one codegen unit,
# which is most of a rebuild's wall time. Fine for release artefacts, painful
# for local iteration; override with LTO=on when cutting a release image.
CARGO_PROFILE_RELEASE_LTO="${LTO:-false}" \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CODEGEN_UNITS:-8}" \
CC_aarch64_unknown_linux_musl="${CC_aarch64_unknown_linux_musl:-musl-gcc}" \
AR_aarch64_unknown_linux_musl="${AR_aarch64_unknown_linux_musl:-ar}" \
"${CARGO:-$HOME/.cargo/bin/cargo}" build --release --locked --target "$TARGET" "$@"

docker build -f Dockerfile.runtime -t "$IMAGE" .

echo "built $IMAGE"
