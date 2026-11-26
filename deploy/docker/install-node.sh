#!/bin/sh
# install-node.sh [version|lts] [prefix]
# Installs Node.js from the official binary release (nodejs.org), verified against the published SHA-256 sums.
#   version  `lts` (default): the newest long-term-support release; or an exact one such as v24.22.3
#   prefix   where it goes (default /usr/local): bin/node, bin/npm, bin/npx, lib/node_modules/npm
# The distribution's own nodejs package is years behind (Debian bookworm ships Node 18); current frameworks need a current Node.
set -eu
want="${1:-lts}"
prefix="${2:-/usr/local}"
case "$(uname -m)" in
  x86_64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "install-node: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
if [ "$want" = "lts" ]; then
  want="$(curl -fsSL https://nodejs.org/dist/index.json | jq -r '[.[] | select(.lts != false)][0].version')"
fi
case "$want" in v[0-9]*.[0-9]*.[0-9]*) ;; *) echo "install-node: '$want' is not a version like v24.22.3" >&2; exit 1 ;; esac
tarball="node-$want-linux-$arch.tar.xz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "https://nodejs.org/dist/$want/$tarball" -o "$tmp/$tarball"
(cd "$tmp" && curl -fsSL "https://nodejs.org/dist/$want/SHASUMS256.txt" | grep " $tarball\$" | sha256sum -c -)
mkdir -p "$prefix"
tar -xJf "$tmp/$tarball" -C "$prefix" --strip-components=1 --no-same-owner --exclude='*/CHANGELOG.md' --exclude='*/LICENSE' --exclude='*/README.md'
echo "install-node: $("$prefix/bin/node" --version), npm $("$prefix/bin/npm" --version) in $prefix"
