#!/usr/bin/env bash
# Installs the Blender that .github/blender.env pins into DIR and prints its executable's path.
# The tarball comes from download.blender.org, or a blender.org mirror when that refuses, and is
# checked against the pinned sha256 either way. An install already in DIR is reused.
#
#   scripts/install-blender.sh ~/.cache/opendam-blender
set -euo pipefail

dir=${1:?usage: install-blender.sh DIR}
# shellcheck source=../.github/blender.env
source "$(dirname "$0")/../.github/blender.env"

name="blender-$BLENDER_VERSION-linux-x64"
bin="$dir/$name/blender"
if [ -x "$bin" ]; then
  echo "$bin"
  exit 0
fi

release="Blender${BLENDER_VERSION%.*}/$name.tar.xz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for base in https://download.blender.org/release \
  https://mirrors.ocf.berkeley.edu/blender/release \
  https://ftp.nluug.nl/pub/graphics/blender/release; do
  echo "Downloading $base/$release" >&2
  curl -fsSL --retry 3 -o "$tmp/blender.tar.xz" "$base/$release" && break
  rm -f "$tmp/blender.tar.xz"
done
if [ ! -f "$tmp/blender.tar.xz" ]; then
  echo "Could not download $release" >&2
  exit 1
fi
echo "$BLENDER_SHA256  $tmp/blender.tar.xz" | sha256sum --check --strict >&2

mkdir -p "$dir"
tar -xJf "$tmp/blender.tar.xz" -C "$dir"
echo "$bin"
