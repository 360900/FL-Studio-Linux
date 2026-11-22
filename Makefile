PREFIX ?= /usr/local
DESTDIR ?=
BINDIR = $(DESTDIR)$(PREFIX)/bin
APPDIR = $(DESTDIR)$(PREFIX)/share/applications
ICONDIR = $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps
METADIR = $(DESTDIR)$(PREFIX)/share/metainfo
LICDIR = $(DESTDIR)$(PREFIX)/share/licenses/fl-studio-linux

.PHONY: build install uninstall appimage

build:
	cargo build --release --locked --workspace

install:
	install -Dm755 target/release/fl-studio-linux $(BINDIR)/fl-studio-linux
	install -Dm755 target/release/fl-studio-linux-gui $(BINDIR)/fl-studio-linux-gui
	install -Dm644 packaging/io.github._360900.FLStudioLinux.desktop $(APPDIR)/io.github._360900.FLStudioLinux.desktop
	install -Dm644 packaging/fl-studio-linux.svg $(ICONDIR)/fl-studio-linux.svg
	install -Dm644 packaging/io.github._360900.FLStudioLinux.metainfo.xml $(METADIR)/io.github._360900.FLStudioLinux.metainfo.xml
	install -Dm644 LICENSE $(LICDIR)/LICENSE

uninstall:
	rm -f $(BINDIR)/fl-studio-linux $(BINDIR)/fl-studio-linux-gui
	rm -f $(APPDIR)/io.github._360900.FLStudioLinux.desktop
	rm -f $(ICONDIR)/fl-studio-linux.svg
	rm -f $(METADIR)/io.github._360900.FLStudioLinux.metainfo.xml
	rm -rf $(LICDIR)

appimage: build
	packaging/appimage/build.sh
