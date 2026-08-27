#!/bin/sh
# Fetch the pinned zig release used as the cross C compiler and linker
# for the Antminer S19K Pro control board (armv7 musl).
#
# Why zig rather than a musl cross-toolchain package: it is a single
# self-contained download that needs no root, no apt/opkg package, and
# no separately built sysroot, and it ships the musl libc headers for
# every target it supports. See docs/s19k-pro/running-it.md.
#
# The version and checksums are pinned here rather than read from
# ziglang.org's index.json, because that index only lists current
# releases -- an older pin would silently stop resolving.
set -eu

ZIG_VERSION=0.16.0
DEST_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/zig

case $(uname -m) in
    x86_64)         ARCH=x86_64  SHA=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00 ;;
    aarch64|arm64)  ARCH=aarch64 SHA=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17 ;;
    *)  echo "fetch-zig: unsupported host architecture $(uname -m)" >&2
        echo "fetch-zig: pick a build from https://ziglang.org/download/ and extract it to $DEST_DIR" >&2
        exit 1 ;;
esac

NAME="zig-${ARCH}-linux-${ZIG_VERSION}"
URL="https://ziglang.org/download/${ZIG_VERSION}/${NAME}.tar.xz"

if [ -x "$DEST_DIR/zig" ] && [ "$("$DEST_DIR/zig" version 2>/dev/null)" = "$ZIG_VERSION" ]; then
    echo "fetch-zig: zig $ZIG_VERSION already present in $DEST_DIR"
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "fetch-zig: downloading $URL"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$TMP/$NAME.tar.xz" "$URL"
elif command -v wget >/dev/null 2>&1; then
    wget -qO "$TMP/$NAME.tar.xz" "$URL"
else
    echo "fetch-zig: need curl or wget" >&2
    exit 1
fi

echo "fetch-zig: verifying checksum"
echo "$SHA  $TMP/$NAME.tar.xz" | sha256sum -c - >/dev/null

echo "fetch-zig: extracting to $DEST_DIR"
rm -rf "$DEST_DIR"
mkdir -p "$DEST_DIR"
# The tarball has a single top-level directory; strip it so the binary
# lands predictably at $DEST_DIR/zig.
tar -xJf "$TMP/$NAME.tar.xz" -C "$DEST_DIR" --strip-components=1

"$DEST_DIR/zig" version >/dev/null
echo "fetch-zig: zig $("$DEST_DIR/zig" version) ready at $DEST_DIR/zig"
