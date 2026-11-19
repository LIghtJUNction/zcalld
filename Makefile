PREFIX ?= /usr
DESTDIR ?=
BINDIR := $(PREFIX)/bin
LIBDIR := $(PREFIX)/lib
DATADIR := $(PREFIX)/share
BUMP ?= patch
VERSION ?=

.PHONY: build check install package bump release
build:
	cargo build --release --workspace --locked

check:
	./scripts/check.sh

install:
	install -Dm755 target/release/zcalld "$(DESTDIR)$(BINDIR)/zcalld"
	install -Dm755 target/release/libzcalld_aliases.so "$(DESTDIR)$(LIBDIR)/zcalld/plugins/libzcalld_aliases.so"
	install -Dm755 target/release/libzcalld_policy.so "$(DESTDIR)$(LIBDIR)/zcalld/plugins/libzcalld_policy.so"
	install -d "$(DESTDIR)$(LIBDIR)/systemd/system" "$(DESTDIR)$(LIBDIR)/systemd/user"
	sed 's|/usr/bin/zcalld|$(BINDIR)/zcalld|g' packaging/systemd/zcalld.service > "$(DESTDIR)$(LIBDIR)/systemd/system/zcalld.service"
	sed 's|/usr/bin/zcalld|$(BINDIR)/zcalld|g' packaging/systemd/user/zcalld.service > "$(DESTDIR)$(LIBDIR)/systemd/user/zcalld.service"
	chmod 644 "$(DESTDIR)$(LIBDIR)/systemd/system/zcalld.service" "$(DESTDIR)$(LIBDIR)/systemd/user/zcalld.service"
	install -Dm644 packaging/man/zcalld.1 "$(DESTDIR)$(DATADIR)/man/man1/zcalld.1"
	install -Dm644 LICENSE "$(DESTDIR)$(DATADIR)/licenses/zcalld/LICENSE"
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
