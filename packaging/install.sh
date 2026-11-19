#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
prefix=${PREFIX:-/usr}
destdir=${DESTDIR:-}
if [[ "$prefix" != /* || "$prefix" == *$'\n'* || "$prefix" == *'|'* ]]; then
  printf 'PREFIX must be an absolute path without newlines or |.\n' >&2
  exit 2
fi
install -Dm755 bin/zcalld "$destdir$prefix/bin/zcalld"
for plugin in lib/zcalld/plugins/*.so; do
  install -Dm755 "$plugin" "$destdir$prefix/lib/zcalld/plugins/${plugin##*/}"
done
for scope in system user; do
  install -d "$destdir$prefix/lib/systemd/$scope"
  sed "s|/usr/bin/zcalld|$prefix/bin/zcalld|g" "lib/systemd/$scope/zcalld.service" > "$destdir$prefix/lib/systemd/$scope/zcalld.service"
  chmod 644 "$destdir$prefix/lib/systemd/$scope/zcalld.service"
done
install -Dm644 share/man/man1/zcalld.1 "$destdir$prefix/share/man/man1/zcalld.1"
install -Dm644 share/licenses/zcalld/LICENSE "$destdir$prefix/share/licenses/zcalld/LICENSE"
install -d "$destdir$prefix/share/doc/zcalld" "$destdir/etc/zcalld"
cp -R share/doc/zcalld/. "$destdir$prefix/share/doc/zcalld/"
chmod -R u=rwX,go=rX "$destdir$prefix/share/doc/zcalld"
if [[ ! -e "$destdir/etc/zcalld/config.conf" ]]; then
  install -m644 etc/zcalld/config.conf "$destdir/etc/zcalld/config.conf"
fi
printf 'Installed zcalld under %s%s.\n' "$destdir" "$prefix"
printf 'Configure the service, then run systemctl daemon-reload and enable the desired service scope.\n'
