//! Application menu entry, FLP association and `flstudio://` handler.

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ico::{IconDir, IconImage};
use pelite::PeFile;

use crate::deps::which;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{LAUNCHER_NAME, WINE_DIR_NAME, app_exe, to_windows_path, wine};

pub const DESKTOP_FILE: &str = "fl-studio.desktop";
pub const URL_HANDLER_FILE: &str = "fl-studio-url.desktop";
pub const MIME_FILE: &str = "fl-studio.xml";
pub const ICON_NAME: &str = "fl-studio";
/// Used when FL Studio's own icon cannot be read from its executable.
const ICON_SVG: &str = include_str!("../../../packaging/fl-studio-linux.svg");
/// Sizes with a fixed directory in the hicolor theme.
const ICON_SIZES: [u32; 13] = [16, 22, 24, 32, 36, 48, 64, 72, 96, 128, 192, 256, 512];
const SCHEME_TYPES: &str = "x-scheme-handler/flstudio;x-scheme-handler/fl-cloud-plugins;";

#[derive(Debug, Clone)]
pub struct DesktopOptions {
    pub prefix: PathBuf,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DesktopOutcome {
    pub desktop: PathBuf,
    pub url_desktop: PathBuf,
    pub icon: PathBuf,
    pub mime: PathBuf,
    pub url_handler: bool,
}

pub fn create<F>(opts: &DesktopOptions, on_line: F) -> Result<DesktopOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let applications = applications_dir()?;
    let desktop = applications.join(DESKTOP_FILE);
    let url_desktop = applications.join(URL_HANDLER_FILE);
    let icon_theme = icon_theme_dir()?;
    let mut icon = scalable_icon(&icon_theme);
    let mime = mime_packages_dir()?.join(MIME_FILE);
    let exec = exec_line(&opts.prefix);
    let contents = desktop_contents(&opts.prefix, &exec);
    let url_contents = url_handler_contents(&exec);

    report.info(format!("desktop entry: {}", desktop.display()));
    if opts.dry_run {
        report.info(format!("    {}", contents.replace('\n', "\n    ")));
        report.info(format!("    url handler: {}", url_desktop.display()));
        report.info(format!("    {}", url_contents.replace('\n', "\n    ")));
        report.info("    icon: extracted from FL64.exe, or the bundled icon if that fails");
        report.info(format!("    mime: {}", mime.display()));
    } else {
        if app_exe(&opts.prefix).is_none() {
            bail!(
                "FL Studio is not installed in {}; cannot create a launcher",
                opts.prefix.display()
            );
        }
        fs::create_dir_all(&applications)?;
        fs::write(&desktop, &contents).with_context(|| format!("writing {}", desktop.display()))?;
        fs::write(&url_desktop, &url_contents)
            .with_context(|| format!("writing {}", url_desktop.display()))?;
        icon = install_icon(&opts.prefix, &icon_theme, &mut report)?;
        fs::create_dir_all(mime.parent().context("mime directory")?)?;
        fs::write(&mime, mime_xml()).with_context(|| format!("writing {}", mime.display()))?;

        if let Some(desktop_folder) = desktop_dir().filter(|dir| dir.is_dir()) {
            let shortcut = desktop_folder.join("FL Studio.desktop");
            if fs::copy(&desktop, &shortcut).is_ok() {
                let _ = fs::set_permissions(&shortcut, fs::Permissions::from_mode(0o755));
                // GNOME's desktop icons only launch shortcuts marked trusted.
                if which("gio").is_some() {
                    let _ = Command::new("gio")
                        .args(["set"])
                        .arg(&shortcut)
                        .args(["metadata::trusted", "true"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }
    }

    let registered = if opts.dry_run {
        register_protocol(opts, &mut report)?
    } else {
        hide_wine_entries(&applications, &opts.prefix, &mut report);
        refresh_databases(&applications, &mut report);
        let registered = register_protocol(opts, &mut report)?;
        refresh_databases(&applications, &mut report);
        registered
    };

    Ok(DesktopOutcome {
        desktop,
        url_desktop,
        icon,
        mime,
        url_handler: registered,
    })
}

/// Installs FL Studio's own icon from its executable, falling back to the
/// bundled one. Icons left by the other source are removed so they cannot win
/// the theme lookup. Returns the largest icon written.
fn install_icon(prefix: &Path, theme: &Path, report: &mut Report<'_>) -> Result<PathBuf> {
    let extracted = app_exe(prefix)
        .context("FL Studio is not installed")
        .and_then(|exe| exe_icons(&exe));
    match extracted {
        Ok(icons) => {
            let mut largest = None;
            for (size, png) in &icons {
                let path = sized_icon(theme, *size);
                fs::create_dir_all(path.parent().context("icon directory")?)?;
                fs::write(&path, png).with_context(|| format!("writing {}", path.display()))?;
                largest = Some(path);
            }
            for size in ICON_SIZES.iter().filter(|size| !icons.contains_key(size)) {
                remove_icon(&sized_icon(theme, *size));
            }
            remove_icon(&scalable_icon(theme));
            report.info(format!("icon: FL Studio's own, {} sizes", icons.len()));
            largest.context("no icon written")
        }
        Err(error) => {
            report.warn(format!(
                "could not read FL Studio's icon ({error:#}); using the bundled one"
            ));
            let path = scalable_icon(theme);
            fs::create_dir_all(path.parent().context("icon directory")?)?;
            fs::write(&path, ICON_SVG).with_context(|| format!("writing {}", path.display()))?;
            for size in ICON_SIZES {
                remove_icon(&sized_icon(theme, size));
            }
            Ok(path)
        }
    }
}

fn remove_icon(path: &Path) {
    if path.symlink_metadata().is_ok() {
        let _ = fs::remove_file(path);
    }
}

/// The application icon of a Windows executable as PNGs keyed by size. Only
/// square images with a hicolor size are kept, the deepest one per size.
fn exe_icons(exe: &Path) -> Result<BTreeMap<u32, Vec<u8>>> {
    let bytes = fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let pe = PeFile::from_bytes(&bytes).context("not a Windows executable")?;
    let resources = pe.resources().context("no resources")?;
    // Windows shows the first icon group as the program's icon.
    let (_, group) = resources
        .icons()
        .find_map(|icon| icon.ok())
        .context("no icon")?;
    let mut best: BTreeMap<u32, (u16, IconImage)> = BTreeMap::new();
    for entry in group.entries() {
        let Ok(image) = group
            .image(entry.nId)
            .map_err(|_| ())
            .and_then(|data| decode_icon(data).map_err(|_| ()))
        else {
            continue;
        };
        let size = image.width();
        if image.height() != size || !ICON_SIZES.contains(&size) {
            continue;
        }
        if best
            .get(&size)
            .is_none_or(|(depth, _)| entry.wBitCount > *depth)
        {
            best.insert(size, (entry.wBitCount, image));
        }
    }
    if best.is_empty() {
        bail!("no usable icon size");
    }
    best.into_iter()
        .map(|(size, (_, image))| {
            let mut png = Vec::new();
            image.write_png(&mut png)?;
            Ok((size, png))
        })
        .collect()
}

/// Decodes one icon image, stored either as a PNG or as a bare bitmap.
fn decode_icon(data: &[u8]) -> std::io::Result<IconImage> {
    if data.starts_with(b"\x89PNG") {
        return IconImage::read_png(Cursor::new(data));
    }
    // Wrap the bitmap in a one-image ICO file so the ico crate can decode it.
    // The size fields are placeholders; it reads the real size from the bitmap.
    let mut file = Vec::with_capacity(22 + data.len());
    file.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    file.extend_from_slice(&[0, 0, 0, 0]);
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    file.extend_from_slice(&22u32.to_le_bytes());
    file.extend_from_slice(data);
    IconDir::read(Cursor::new(file))?.entries()[0].decode()
}

fn desktop_contents(prefix: &Path, exec: &str) -> String {
    let path_value = escape_desktop_string(&prefix.display().to_string());
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=FL Studio\n\
         GenericName=Digital Audio Workstation\n\
         Comment=Windows FL Studio running through Wine\n\
         Icon={ICON_NAME}\n\
         Path={path_value}\n\
         Exec={exec} %F\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Midi;\n\
         MimeType=application/x-flp;\n\
         StartupNotify=true\n\
         StartupWMClass=fl64.exe\n"
    )
}

fn url_handler_contents(exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=FL Studio link handler\n\
         Comment=Open flstudio:// sign-in links in FL Studio\n\
         Exec={exec} %u\n\
         MimeType={SCHEME_TYPES}\n\
         NoDisplay=true\n\
         Terminal=false\n"
    )
}

fn mime_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-flp">
    <comment>FL Studio project</comment>
    <glob pattern="*.flp"/>
    <icon name="fl-studio"/>
  </mime-type>
</mime-info>
"#
}

/// Starts FL Studio through our own launcher, so menu launches get the same
/// environment as the Open button. Falls back to plain Wine when the launcher
/// binary cannot be found.
fn exec_line(prefix: &Path) -> String {
    match launcher() {
        Some(cli) => launcher_exec(&cli, prefix),
        None => wine_exec_line(prefix),
    }
}

fn launcher() -> Option<PathBuf> {
    let current = std::env::current_exe().ok();
    // AppRun forwards any arguments to the CLI. Only trust an AppImage that is
    // this project: other AppImages in the environment set APPIMAGE too.
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        let path = PathBuf::from(appimage);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let mounted = current
            .as_ref()
            .map(|exe| exe.to_string_lossy().contains(".mount_"))
            .unwrap_or(false);
        if name.contains("fl_studio_linux") || name.contains("fl-studio-linux") || mounted {
            return Some(path);
        }
    }
    if let Some(current) = current {
        let cli = current.with_file_name(LAUNCHER_NAME);
        if cli.is_file() {
            return Some(cli);
        }
        if current
            .file_name()
            .map(|name| name == LAUNCHER_NAME)
            .unwrap_or(false)
        {
            return Some(current);
        }
    }
    which(LAUNCHER_NAME)
}

fn launcher_exec(cli: &Path, prefix: &Path) -> String {
    format!(
        "{} launch --prefix {}",
        quote(&cli.display().to_string()),
        quote(&prefix.display().to_string())
    )
}

fn wine_exec_line(prefix: &Path) -> String {
    let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
    let exe =
        app_exe(prefix).unwrap_or_else(|| crate::app_dir(prefix).join("FL Studio 2026/FL64.exe"));
    format!(
        "env WINEPREFIX={} {} {}",
        quote(&prefix.display().to_string()),
        quote(&wine.display().to_string()),
        quote(&exe.display().to_string())
    )
}

fn register_protocol(opts: &DesktopOptions, report: &mut Report<'_>) -> Result<bool> {
    let exe = app_exe(&opts.prefix)
        .unwrap_or_else(|| crate::app_dir(&opts.prefix).join("FL Studio 2026/FL64.exe"));
    let target = format!("\"{}\" \"%1\"", to_windows_path(&exe));
    let entries = [
        ("HKCU\\Software\\Classes\\flstudio", "URL Protocol", ""),
        (
            "HKCU\\Software\\Classes\\flstudio\\shell\\open\\command",
            "",
            target.as_str(),
        ),
        ("HKCU\\Software\\Classes\\.flp", "", "FLStudio.Project"),
        (
            "HKCU\\Software\\Classes\\FLStudio.Project",
            "",
            "FL Studio project",
        ),
        (
            "HKCU\\Software\\Classes\\FLStudio.Project\\shell\\open\\command",
            "",
            target.as_str(),
        ),
    ];

    let wine_dir = opts.prefix.join(WINE_DIR_NAME);
    let wine_bin = wine_dir.join("bin/wine");
    report.info("flstudio:// and .flp handlers in the Wine prefix");
    for (key, name, value) in entries {
        let mut cmd = Command::new(&wine_bin);
        cmd.args(["reg", "add", key]);
        if name.is_empty() {
            cmd.arg("/ve");
        } else {
            cmd.args(["/v", name]);
        }
        cmd.args(["/d", value, "/f"]);
        if opts.dry_run {
            let value_flag = if name.is_empty() {
                "/ve".to_string()
            } else {
                format!("/v '{name}'")
            };
            report.info(format!(
                "    WINEPREFIX={} {} reg add {} {value_flag} /d '{}' /f",
                opts.prefix.display(),
                wine_bin.display(),
                key,
                value
            ));
            continue;
        }
        if !wine_bin.is_file() {
            report.warn("skipping Wine handlers: Wine is not installed");
            return Ok(false);
        }
        let mut options = RunOptions::default().stall(Duration::from_secs(120));
        options.envs = wine::wine_env(&opts.prefix, &wine_dir, "-all");
        let result = runner::run(cmd, options, |stream, line| report.line(stream, line))?;
        if !result.success() {
            report.warn(format!("could not register {key}; continuing"));
        }
    }

    if !opts.dry_run {
        let defaults = [
            (DESKTOP_FILE, "application/x-flp"),
            (URL_HANDLER_FILE, "x-scheme-handler/flstudio"),
            (URL_HANDLER_FILE, "x-scheme-handler/fl-cloud-plugins"),
        ];
        if which("xdg-mime").is_none() {
            report.warn("xdg-mime is missing; the desktop handlers are not registered");
            return Ok(false);
        }
        let mut all_ok = true;
        for (file, scheme) in defaults {
            let mut cmd = Command::new("xdg-mime");
            cmd.args(["default", file, scheme]);
            match runner::run(
                cmd,
                RunOptions::default().stall(Duration::from_secs(60)).quiet(),
                |stream, line| report.line(stream, line),
            ) {
                Ok(result) if result.success() => {}
                Ok(result) => {
                    all_ok = false;
                    report.warn(format!(
                        "xdg-mime default {file} {scheme} failed ({})",
                        result.status
                    ));
                }
                Err(error) => {
                    all_ok = false;
                    report.warn(format!("xdg-mime default {file} {scheme} failed: {error}"));
                }
            }
        }
        return Ok(all_ok);
    } else {
        for (file, scheme) in [
            (DESKTOP_FILE, "application/x-flp"),
            (URL_HANDLER_FILE, "x-scheme-handler/flstudio"),
            (URL_HANDLER_FILE, "x-scheme-handler/fl-cloud-plugins"),
        ] {
            report.info(format!("    xdg-mime default {file} {scheme}"));
        }
    }
    Ok(true)
}

/// Wine is told not to create menu entries, but older installs may have left
/// some behind. Hide this prefix's ones so the menu does not offer a launcher
/// that bypasses our environment; other prefixes' entries stay as they are.
fn hide_wine_entries(applications: &Path, prefix: &Path, report: &mut Report<'_>) {
    let prefixes = [prefix.to_path_buf()];
    let roots = [
        applications.join("wine/Programs/Image-Line"),
        applications.join("wine-protocol-flstudio.desktop"),
        applications.join("wine-protocol-fl-cloud-plugins.desktop"),
    ];
    for root in roots {
        if root.is_file() {
            hide_entry(&root, &prefixes, report);
        } else if root.is_dir() {
            hide_dir(&root, &prefixes, report);
        }
    }
}

fn hide_dir(dir: &Path, prefixes: &[PathBuf], report: &mut Report<'_>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            hide_dir(&path, prefixes, report);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            hide_entry(&path, prefixes, report);
        }
    }
}

fn hide_entry(path: &Path, prefixes: &[PathBuf], report: &mut Report<'_>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    if !crate::uninstall::references(&text, prefixes)
        || text.lines().any(|line| line.trim() == "NoDisplay=true")
    {
        return;
    }
    let mut text = text;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("NoDisplay=true\n");
    if fs::write(path, text).is_ok() {
        report.info(format!("hid duplicate menu entry {}", path.display()));
    }
}

fn refresh_databases(applications: &Path, report: &mut Report<'_>) {
    if which("update-desktop-database").is_some() {
        let mut cmd = Command::new("update-desktop-database");
        cmd.arg(applications);
        let _ = runner::run(
            cmd,
            RunOptions::default().stall(Duration::from_secs(60)).quiet(),
            |stream, line| report.line(stream, line),
        );
    }
    if let Ok(mime) = mime_root()
        && which("update-mime-database").is_some()
    {
        let mut cmd = Command::new("update-mime-database");
        cmd.arg(mime);
        let _ = runner::run(
            cmd,
            RunOptions::default().stall(Duration::from_secs(60)).quiet(),
            |stream, line| report.line(stream, line),
        );
    }
}

fn applications_dir() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("applications"))
}

fn icon_theme_dir() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("icons/hicolor"))
}

fn scalable_icon(theme: &Path) -> PathBuf {
    theme.join("scalable/apps").join(format!("{ICON_NAME}.svg"))
}

fn sized_icon(theme: &Path, size: u32) -> PathBuf {
    theme
        .join(format!("{size}x{size}/apps"))
        .join(format!("{ICON_NAME}.png"))
}

/// Every icon file `create` may have written under a data directory.
pub(crate) fn icon_paths(data: &Path) -> Vec<PathBuf> {
    let theme = data.join("icons/hicolor");
    let mut paths = vec![scalable_icon(&theme)];
    paths.extend(ICON_SIZES.iter().map(|size| sized_icon(&theme, *size)));
    paths
}

fn mime_root() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("mime"))
}

fn mime_packages_dir() -> Result<PathBuf> {
    Ok(mime_root()?.join("packages"))
}

fn xdg_data_home() -> Result<PathBuf> {
    Ok(crate::config::data_home())
}

/// Resolve the user's desktop directory via XDG user-dirs, falling back to `~/Desktop`.
pub(crate) fn desktop_dir() -> Option<PathBuf> {
    if which("xdg-user-dir").is_some()
        && let Ok(output) = Command::new("xdg-user-dir").arg("DESKTOP").output()
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            let path = PathBuf::from(&value);
            if home_dir().ok().as_deref() != Some(path.as_path()) {
                return Some(path);
            }
        }
    }
    let home = home_dir().ok()?;
    let config_home = crate::config::config_home();
    if let Ok(text) = fs::read_to_string(config_home.join("user-dirs.dirs"))
        && let Some(dir) = parse_user_dir(&text, "XDG_DESKTOP_DIR", &home)
    {
        return Some(dir);
    }
    Some(home.join("Desktop"))
}

fn parse_user_dir(text: &str, key: &str, home: &Path) -> Option<PathBuf> {
    let prefix = format!("{key}=");
    for line in text.lines() {
        let line = line.trim();
        let Some(raw) = line.strip_prefix(&prefix) else {
            continue;
        };
        let value = raw.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        let path = if let Some(rest) = value.strip_prefix("$HOME/") {
            home.join(rest)
        } else if value == "$HOME" {
            return None;
        } else {
            PathBuf::from(value)
        };
        return Some(path);
    }
    None
}

fn home_dir() -> Result<PathBuf> {
    Ok(crate::config::home_dir())
}

/// Escape a `.desktop` string value (general string escaping).
fn escape_desktop_string(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Quote a single argument for a `.desktop` `Exec=` line per the Freedesktop
/// Desktop Entry Specification.
pub fn quote(text: &str) -> String {
    let escaped_percent = text.replace('%', "%%");
    if !escaped_percent.is_empty()
        && escaped_percent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+%".contains(c))
    {
        return escaped_percent;
    }
    let mut out = String::with_capacity(escaped_percent.len() + 2);
    out.push('"');
    for ch in escaped_percent.chars() {
        match ch {
            // Desktop string unescaping runs before Exec quoting, so backslashes
            // and Exec-reserved characters are escaped twice.
            '\\' => out.push_str("\\\\\\\\"),
            '"' => out.push_str("\\\\\""),
            '`' => out.push_str("\\\\`"),
            '$' => out.push_str("\\\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_handles_projects_and_links() {
        let contents = desktop_contents(Path::new("/home/me/Prefix"), "fl-studio-linux launch");
        assert!(contents.contains("Exec=fl-studio-linux launch %F"));
        assert!(contents.contains("StartupWMClass=fl64.exe"));
        assert!(contents.contains("application/x-flp"));
        let handler = url_handler_contents("fl-studio-linux launch");
        assert!(handler.contains("Exec=fl-studio-linux launch %u"));
        assert!(handler.contains("x-scheme-handler/flstudio"));
        assert!(handler.contains("x-scheme-handler/fl-cloud-plugins"));
        assert!(handler.contains("NoDisplay=true"));
    }

    #[test]
    fn exec_goes_through_the_launcher() {
        let exec = launcher_exec(
            Path::new("/usr/bin/fl-studio-linux"),
            Path::new("/home/me/My Prefix"),
        );
        assert_eq!(
            exec,
            "/usr/bin/fl-studio-linux launch --prefix \"/home/me/My Prefix\""
        );
    }

    #[test]
    fn quote_escapes_desktop_exec_characters() {
        assert_eq!(quote("/home/me/100%"), "/home/me/100%%");
        assert_eq!(
            quote("/home/me/My $Dir \"a\"\\b"),
            "\"/home/me/My \\\\$Dir \\\\\"a\\\\\"\\\\\\\\b\""
        );
    }

    #[test]
    fn parses_localized_desktop_dir() {
        let home = Path::new("/home/me");
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\n";
        assert_eq!(
            parse_user_dir(text, "XDG_DESKTOP_DIR", home),
            Some(PathBuf::from("/home/me/Schreibtisch"))
        );
    }

    #[test]
    fn wine_fallback_uses_the_newest_executable() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app_dir(dir.path()).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();
        let line = wine_exec_line(dir.path());
        assert!(line.contains("FL64.exe"));
        assert!(line.contains("WINEPREFIX="));
    }

    #[test]
    fn dry_run_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let options = DesktopOptions {
            prefix: dir.path().to_path_buf(),
            dry_run: true,
            log_path: None,
        };
        let mut lines = Vec::new();
        create(&options, |_, line| lines.push(line.to_string())).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("Exec="));
        assert!(text.contains("flstudio"));
        assert!(text.contains("/ve"));
        assert!(text.contains("xdg-mime default"));
        assert!(!dir.path().join("FL Studio.desktop").exists());
    }

    #[test]
    fn duplicate_wine_entries_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let applications = dir.path().join("applications");
        let entry = applications.join("wine/Programs/Image-Line/FL Studio/FL Studio.desktop");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(
            &entry,
            "[Desktop Entry]\nName=FL Studio\nExec=env WINEPREFIX=\"/home/me/.FLStudioLinux\" wine FL64.exe\n",
        )
        .unwrap();
        let other = applications.join("wine/Programs/Image-Line/FL Studio/Other.desktop");
        fs::write(
            &other,
            "[Desktop Entry]\nName=FL Studio\nExec=env WINEPREFIX=\"/home/me/Bottles/FL\" wine FL64.exe\n",
        )
        .unwrap();
        let mut lines = Vec::new();
        let mut sink = |_, line: &str| lines.push(line.to_string());
        let mut report = Report::new(None, &mut sink);
        hide_wine_entries(
            &applications,
            Path::new("/home/me/.FLStudioLinux"),
            &mut report,
        );
        assert!(
            fs::read_to_string(&entry)
                .unwrap()
                .contains("NoDisplay=true")
        );
        assert!(
            !fs::read_to_string(&other)
                .unwrap()
                .contains("NoDisplay=true")
        );
    }

    #[test]
    fn decodes_png_and_bitmap_icons() {
        let mut image = IconImage::from_rgba_data(32, 32, vec![0; 32 * 32 * 4]);
        image.rgba_data_mut()[..4].copy_from_slice(&[255, 120, 0, 255]);
        let png = ico::IconDirEntry::encode_as_png(&image).unwrap();
        let bmp = ico::IconDirEntry::encode_as_bmp(&image).unwrap();
        for data in [png.data(), bmp.data()] {
            let decoded = decode_icon(data).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (32, 32));
            assert_eq!(decoded.rgba_data(), image.rgba_data());
        }
    }

    #[test]
    fn non_executables_have_no_icon() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("FL64.exe");
        fs::write(&exe, b"not a PE file").unwrap();
        assert!(exe_icons(&exe).is_err());
    }

    #[test]
    fn falls_back_to_the_bundled_icon() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app_dir(dir.path()).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();
        let theme = dir.path().join("icons/hicolor");
        let stale = sized_icon(&theme, 48);
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"png").unwrap();
        let mut lines = Vec::new();
        let mut sink = |_, line: &str| lines.push(line.to_string());
        let mut report = Report::new(None, &mut sink);
        let icon = install_icon(dir.path(), &theme, &mut report).unwrap();
        assert_eq!(icon, scalable_icon(&theme));
        assert_eq!(fs::read_to_string(&icon).unwrap(), ICON_SVG);
        assert!(!stale.exists());
        assert!(lines.iter().any(|line| line.contains("bundled")));
    }

    #[test]
    fn icon_paths_cover_every_size() {
        let paths = icon_paths(Path::new("/data"));
        assert!(paths.contains(&PathBuf::from(
            "/data/icons/hicolor/scalable/apps/fl-studio.svg"
        )));
        assert!(paths.contains(&PathBuf::from(
            "/data/icons/hicolor/256x256/apps/fl-studio.png"
        )));
    }

    #[test]
    fn mime_definition_covers_flp_files() {
        assert!(mime_xml().contains("*.flp"));
    }
}
