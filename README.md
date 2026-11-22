# FL Studio Linux

![FL Studio Linux installer](docs/img/fl-studio-linux-gui.png)

One-click installer and launcher for FL Studio on Linux. It creates a private
Wine prefix, installs the official Windows build from Image-Line, adds the
fonts and plugin runtimes FL Studio expects, applies the WebView2 workarounds
and creates a menu entry that always launches the right environment.

Not affiliated with or endorsed by Image-Line. FL Studio is proprietary
software; the installer downloads the official trial and you unlock it with
your Image-Line account.

## Features

- Private Wine 11.22 build (Kron4ek) with Wine 11.21 as a fallback. FL Studio
  26.1.4 and later need Wine 11.21 or newer for the signature check.
- System dependencies for Arch, Ubuntu-family, Fedora and openSUSE
- Microsoft core fonts (Arial, Verdana and the rest) plus MS Gothic for
  musical symbols, so FLEX and the piano roll render correctly
- Visual C++, GDI+ and shader compiler runtimes for Windows plugins
- Optional DXVK, off by default because it can affect WebView2 panes
- WebView2 registry fixes, disabled Edge updater
- Automatic Wine DPI from the display's resolution and scale
- FLP association and `flstudio://` sign-in handler
- Update, repair and uninstall, live log and visible errors
- Every action supports `--dry-run`

## Install

Packages are attached to each [release](https://github.com/360900/FL-Studio-Linux/releases).

Arch and derivatives:

```sh
git clone https://github.com/360900/FL-Studio-Linux
cd FL-Studio-Linux/packaging/arch && makepkg -si
```

Debian, Ubuntu and derivatives:

```sh
sudo apt install ./fl-studio-linux_<version>-1_amd64.deb
```

Fedora:

```sh
sudo dnf install ./fl-studio-linux-<version>-1.x86_64.rpm
```

openSUSE:

```sh
sudo zypper install ./fl-studio-linux-<version>-1.x86_64.rpm
```

Any distro, AppImage (no arguments opens the GUI, arguments go to the CLI):

```sh
chmod +x FL_Studio_Linux-<version>-x86_64.AppImage
./FL_Studio_Linux-<version>-x86_64.AppImage
./FL_Studio_Linux-<version>-x86_64.AppImage install --dry-run
```

Other distros, from the release tarball (or after `make build`):

```sh
sudo make install            # PREFIX=/usr/local by default
sudo make uninstall
```

## Build

Requires Rust 1.88+.

```sh
make build                   # cargo build --release --locked --workspace
```

Building the GUI needs fontconfig headers (`libfontconfig1-dev`,
`fontconfig-devel`).

## Usage

GUI:

```sh
fl-studio-linux-gui                          # installed
cargo run --release -p fl-studio-linux-gui   # from a source checkout
```

CLI:

```sh
fl-studio-linux install     # --installer <setup.exe>, --dxvk true|false, --fonts true|false, --dpi <n>
fl-studio-linux launch      # project.flp or flstudio://... args are forwarded
fl-studio-linux update
fl-studio-linux repair      # missing fonts, prefix settings, DXVK, launcher
fl-studio-linux uninstall
```

`--dpi` takes 96 to 480 (96 = 100%) and defaults to the display's DPI. Add
`--dry-run` to preview changes and `--yes` to skip prompts. Hidden
diagnostics: `doctor`, `deps`, `wine-setup`.

The installation starts in trial mode. Sign in with your Image-Line account
inside FL Studio to unlock a paid edition.

## Notes

- FL Studio 26.1.4 and later need Wine 11.21 or newer. Do not switch the
  prefix to an older runner.
- DXVK is off by default. It helps slow Direct3D plugins but can darken or
  break FL Cloud, Help and the Sounds tab. Toggle it in Options and repair.
- If FL Studio lists PipeWire MIDI ports as `FAIL`, open `Options > MIDI
  Settings` and disable those inputs. They are virtual ports, not hardware.
- The installer starts Edge WebView2 near the end and can look stuck while its
  services keep running. The watchdog stops the prefix after FL64.exe has been
  stable for a while.
- 32-bit plugins are fragile under Wine. Prefer the 64-bit versions.
- On immutable distributions such as Bazzite, install `winetricks`,
  `cabextract` and the Wine libraries with `rpm-ostree` first. The installer
  warns and continues when no package plan exists, but fonts need winetricks.
- Run the installer as your normal user. It asks for elevation only when it
  installs system packages; running the whole app with `sudo` would create the
  prefix and menu entries in root's home.
- FL Studio is not officially supported on Linux. Keep project backups.

## Files

- Config: `~/.config/fl-studio-linux/config.toml` (respects `XDG_CONFIG_HOME`)
- Wine prefix: `~/.FLStudioLinux` (override with `--prefix`)
- Logs: `<prefix>/logs` and `~/.cache/fl-studio-linux/launch.log`
- Download cache: `~/.cache/fl-studio-linux` (respects `XDG_CACHE_HOME`)

## Layout

| Path | Purpose |
| --- | --- |
| `crates/core` | Headless engine |
| `crates/cli` | `fl-studio-linux` CLI, used by the desktop entry |
| `crates/app` | Slint GUI |
| `ui/main.slint` | Window layout |
| `packaging` | Desktop entry, icon, PKGBUILD, AppImage |

## Credits

- [Torbuntu/fl-studio-linux-setup](https://github.com/Torbuntu/fl-studio-linux-setup)
  for the Bottles setup notes, font requirements and WebView2 findings
- [FL Studio 2026 on Linux](https://pr1v8.ca/posts/fl-studio-wine-plugins-ilok/)
  for the WebView2 Windows 8 workaround and plugin runtimes
- [FL Studio on Linux in 2026](https://blog.mrquantumoff.dev/fl-studio-on-linux-in-2026-is-very-much-usable-with-vsts-too-with-wine/)
  for the font, runtime and NVIDIA vsync notes
- [Kron4ek/Wine-Builds](https://github.com/Kron4ek/Wine-Builds) for the Wine builds
- WineHQ bug 60273 and the Wine 11.21 release for the FL Studio signature fix

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
