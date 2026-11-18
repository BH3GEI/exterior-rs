#!/bin/sh
# Build the static Linux binary and bundle the Alpine packages it needs (ssh, sshpass, git, curl).
# Nothing is downloaded at run time; apk checks every package's signature against the image's keys.
set -eu
cd "$(dirname "$0")"
( cd .. && CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target x86_64-unknown-linux-musl )
[ -n "$(ls ../vendor-apk/*.apk 2>/dev/null)" ] || python3 fetch-apk.py
rm -rf content/apk && mkdir -p content/apk && cp ../vendor-apk/*.apk content/apk/
cp ../target/x86_64-unknown-linux-musl/release/exterior content/exterior
