PREFIX ?= /usr
DESTDIR ?=
BINDIR := $(PREFIX)/bin
LIBDIR := $(PREFIX)/lib
DATADIR := $(PREFIX)/share
BUMP ?= patch
VERSION ?=
PLUGINS := zcalld_aliases zcalld_policy zcalld_redact zcalld_community zcalld_plugin_routing zcalld_desktop

.PHONY: build check check-native install package bump release
build:
	cargo build --release --workspace --locked

check:
	./scripts/check.sh

# Optional provider/privacy acceptance needs Go and Bun; core-only checks do not.
ZCALLD_NATIVE_EXE = $(shell go env GOEXE)
check-native: build
	mkdir -p _release/native-compat
	go -C extensions/compat build -mod=readonly -trimpath -o "../../_release/native-compat/zcalld-pluginctl$(ZCALLD_NATIVE_EXE)" ./cmd/zcalld-pluginctl
	go -C extensions/compat build -mod=readonly -trimpath -o "../../_release/native-compat/zcalld-privacy$(ZCALLD_NATIVE_EXE)" ./cmd/zcalld-privacy
	python3 scripts/test-native-compat.py

install:
	install -Dm755 target/release/zcalld "$(DESTDIR)$(BINDIR)/zcalld"
	install -Dm755 target/release/zcalld-installer "$(DESTDIR)$(BINDIR)/zcalld-installer"
	@for plugin in $(PLUGINS); do install -Dm755 "target/release/lib$$plugin.so" "$(DESTDIR)$(LIBDIR)/zcalld/plugins/lib$$plugin.so"; done
	install -d "$(DESTDIR)$(LIBDIR)/systemd/system" "$(DESTDIR)$(LIBDIR)/systemd/user"
	sed 's|/usr/bin/zcalld|$(BINDIR)/zcalld|g' packaging/systemd/zcalld.service > "$(DESTDIR)$(LIBDIR)/systemd/system/zcalld.service"
	sed 's|/usr/bin/zcalld|$(BINDIR)/zcalld|g' packaging/systemd/user/zcalld.service > "$(DESTDIR)$(LIBDIR)/systemd/user/zcalld.service"
	chmod 644 "$(DESTDIR)$(LIBDIR)/systemd/system/zcalld.service" "$(DESTDIR)$(LIBDIR)/systemd/user/zcalld.service"
	install -Dm644 packaging/sysusers.d/zcalld.conf "$(DESTDIR)$(LIBDIR)/sysusers.d/zcalld.conf"
	install -Dm644 packaging/tmpfiles.d/zcalld.conf "$(DESTDIR)$(LIBDIR)/tmpfiles.d/zcalld.conf"
	install -Dm644 packaging/man/zcalld.1 "$(DESTDIR)$(DATADIR)/man/man1/zcalld.1"
	install -Dm644 LICENSE "$(DESTDIR)$(DATADIR)/licenses/zcalld/LICENSE"
	install -Dm644 packaging/licenses/zstd.txt "$(DESTDIR)$(DATADIR)/licenses/zcalld/zstd.txt"
	install -Dm644 packaging/licenses/zlib.txt "$(DESTDIR)$(DATADIR)/licenses/zcalld/zlib.txt"
	install -Dm644 README.md "$(DESTDIR)$(DATADIR)/doc/zcalld/README.md"
	cp -R docs "$(DESTDIR)$(DATADIR)/doc/zcalld/"
	chmod -R u=rwX,go=rX "$(DESTDIR)$(DATADIR)/doc/zcalld/docs"
	install -Dm644 examples/config.conf "$(DESTDIR)$(DATADIR)/doc/zcalld/config.conf"
	install -d "$(DESTDIR)/etc/zcalld"
	@if ! test -e "$(DESTDIR)/etc/zcalld/config.conf"; then install -m644 examples/config.conf "$(DESTDIR)/etc/zcalld/config.conf"; fi

package: build
	./scripts/package.sh --no-build

bump:
	python3 scripts/release.py bump "$(BUMP)" $(if $(VERSION),--version "$(VERSION)") --commit --tag

release:
	python3 scripts/release.py push
