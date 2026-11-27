#!/usr/bin/env bash
set -euo pipefail

repo="kmiller96/fledgling"
release_url="https://github.com/$repo/releases/latest/download"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Linux) os_target="unknown-linux-gnu" ;;
  Darwin) os_target="apple-darwin" ;;
  *)
    echo "Unsupported operating system: $os" >&2
    exit 1
    ;;
esac

case "$arch" in
  x86_64|amd64) rust_arch="x86_64" ;;
  arm64|aarch64)
    if [[ "$os" != "Darwin" ]]; then
      echo "Linux ARM64 binaries are not available yet." >&2
      exit 1
    fi
    rust_arch="aarch64"
    ;;
  *)
    echo "Unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

target="${rust_arch}-${os_target}"
archive="fledgling-${target}.tar.gz"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

curl --fail --location --silent --show-error \
  "$release_url/$archive" -o "$tmpdir/$archive"
tar -xzf "$tmpdir/$archive" -C "$tmpdir"

install_dir="${FLEDGLING_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
install "$tmpdir/fledgling" "$install_dir/fledgling"

echo "Installed fledgling to $install_dir/fledgling"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "Add $install_dir to your PATH to run fledgling." ;;
esac
