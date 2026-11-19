#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=target
if [[ "${1:-}" == --no-build ]]; then
  shift
else
  cargo build --release --workspace --locked
fi
if (( $# != 0 )); then
  printf 'Usage: scripts/package.sh [--no-build]\n' >&2
  exit 2
fi
arch=$(uname -m)
case "$arch" in
  x86_64|aarch64) ;;
  *) printf 'Unsupported release architecture: %s\n' "$arch" >&2; exit 2 ;;
esac
version=$(python3 scripts/release.py version)
stem="zcalld-$version-$arch-unknown-linux-gnu"
output="$PWD/dist"
mkdir -p "$output"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
make install DESTDIR="$stage/$stem" PREFIX=/usr
mv "$stage/$stem/usr/bin" "$stage/$stem/bin"
mv "$stage/$stem/usr/lib" "$stage/$stem/lib"
mv "$stage/$stem/usr/share" "$stage/$stem/share"
rmdir "$stage/$stem/usr"
install -m755 packaging/install.sh "$stage/$stem/install.sh"
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner -C "$stage" -cf - "$stem" | gzip -n > "$output/$stem.tar.gz"
printf '%s\n' "$output/$stem.tar.gz"
