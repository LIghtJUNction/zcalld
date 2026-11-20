#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if [[ $(uname -s) == Darwin ]]; then default_prefix=/usr/local; else default_prefix=/usr; fi
prefix=${PREFIX:-$default_prefix}
destdir=${DESTDIR:-}
if [[ "$prefix" != /* || "$prefix" == *$'\n'* || "$prefix" == *'|'* ]]; then
  printf 'PREFIX must be an absolute path without newlines or |.\n' >&2
  exit 2
fi
install_file() {
  mkdir -p "$(dirname "$3")"
  install -m"$1" "$2" "$3"
}
install_file 755 bin/zcalld "$destdir$prefix/bin/zcalld"
for plugin in lib/zcalld/plugins/*.so lib/zcalld/plugins/*.dylib; do
  [[ -f "$plugin" ]] || continue
  install_file 755 "$plugin" "$destdir$prefix/lib/zcalld/plugins/${plugin##*/}"
done
if [[ -d libexec/zcalld/zcalld-desktop-notify.app ]]; then
  app=libexec/zcalld/zcalld-desktop-notify.app
  install_file 755 "$app/Contents/MacOS/zcalld-desktop-notify" "$destdir$prefix/$app/Contents/MacOS/zcalld-desktop-notify"
  install_file 644 "$app/Contents/Info.plist" "$destdir$prefix/$app/Contents/Info.plist"
fi
if [[ -d lib/systemd ]]; then
for scope in system user; do
  install -d "$destdir$prefix/lib/systemd/$scope"
  sed "s|/usr/bin/zcalld|$prefix/bin/zcalld|g" "lib/systemd/$scope/zcalld.service" > "$destdir$prefix/lib/systemd/$scope/zcalld.service"
  chmod 644 "$destdir$prefix/lib/systemd/$scope/zcalld.service"
done
for kind in sysusers.d tmpfiles.d; do
  install_file 644 "lib/$kind/zcalld.conf" "$destdir$prefix/lib/$kind/zcalld.conf"
done
mkdir -p "$destdir/etc/zcalld"
if [[ ! -e "$destdir/etc/zcalld/config.conf" ]]; then
  install -m644 etc/zcalld/config.conf "$destdir/etc/zcalld/config.conf"
fi
fi
install_file 644 share/man/man1/zcalld.1 "$destdir$prefix/share/man/man1/zcalld.1"
install_file 644 share/licenses/zcalld/LICENSE "$destdir$prefix/share/licenses/zcalld/LICENSE"
mkdir -p "$destdir$prefix/share/doc/zcalld"
cp -R share/doc/zcalld/. "$destdir$prefix/share/doc/zcalld/"
chmod -R u=rwX,go=rX "$destdir$prefix/share/doc/zcalld"
printf 'Installed zcalld under %s%s.\n' "$destdir" "$prefix"
printf 'Initialize your configuration with zcalld init, then enable the desired service scope.\n'
