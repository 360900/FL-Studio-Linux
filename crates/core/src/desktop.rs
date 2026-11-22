//! Application menu entry, FLP association and `flstudio://` handler.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::deps::which;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{LAUNCHER_NAME, WINE_DIR_NAME, app_exe, to_windows_path};

pub const DESKTOP_FILE: &str = "fl-studio.desktop";
pub const URL_HANDLER_FILE: &str = "fl-studio-url.desktop";
pub const MIME_FILE: &str = "fl-studio.xml";
pub const ICON_FILE: &str = "fl-studio.svg";
const ICON_SVG: &str = include_str!("../../../packaging/fl-studio-linux.svg");
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
    let icon = icons_dir()?.join(ICON_FILE);
    let mime = mime_packages_dir()?.join(MIME_FILE);
    let exec = exec_line(&opts.prefix);
    let contents = desktop_contents(&opts.prefix, &exec);
    let url_contents = url_handler_contents(&exec);

    report.info(format!("desktop entry: {}", desktop.display()));
    if opts.dry_run {
        report.info(format!("    {}", contents.replace('\n', "\n    ")));
        report.info(format!("    url handler: {}", url_desktop.display()));
        report.info(format!("    {}", url_contents.replace('\n', "\n    ")));
        report.info(format!("    icon: {}", icon.display()));
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
        fs::create_dir_all(icon.parent().context("icon directory")?)?;
        fs::write(&icon, ICON_SVG).with_context(|| format!("writing {}", icon.display()))?;
        fs::create_dir_all(mime.parent().context("mime directory")?)?;
        fs::write(&mime, mime_xml()).with_context(|| format!("writing {}", mime.display()))?;

        let shortcut = home_dir()?.join("Desktop").join("FL Studio.desktop");
        if shortcut
            .parent()
            .map(|parent| parent.is_dir())
            .unwrap_or(false)
        {
            let _ = fs::copy(&desktop, &shortcut);
        }
    }

    let registered = if opts.dry_run {
        register_protocol(opts, &mut report)?
    } else {
        hide_wine_entries(&applications, &mut report);
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

fn desktop_contents(prefix: &Path, exec: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=FL Studio\n\
         GenericName=Digital Audio Workstation\n\
         Comment=Windows FL Studio running through Wine\n\
         Icon=fl-studio\n\
         Path={}\n\
         Exec={exec} %F\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Midi;\n\
         MimeType=application/x-flp;\n\
         StartupNotify=true\n\
         StartupWMClass=fl64.exe\n",
        prefix.display()
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
    let current = current?;
    let cli = current.with_file_name(LAUNCHER_NAME);
    if cli.is_file() {
        return Some(cli);
    }
    current
        .file_name()
        .map(|name| name == LAUNCHER_NAME)
        .unwrap_or(false)
        .then_some(current)
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

    report.info("flstudio:// and .flp handlers in the Wine prefix");
    for (key, name, value) in entries {
        let mut cmd = Command::new(opts.prefix.join(WINE_DIR_NAME).join("bin/wine"));
        cmd.args(["reg", "add", key, "/v", name, "/d", value, "/f"]);
        if opts.dry_run {
            report.info(format!(
                "    WINEPREFIX={} {} reg add {} /v '{}' /d '{}' /f",
                opts.prefix.display(),
                opts.prefix.join(WINE_DIR_NAME).join("bin/wine").display(),
                key,
                name,
                value
            ));
            continue;
        }
        if !opts.prefix.join(WINE_DIR_NAME).join("bin/wine").is_file() {
            report.warn("skipping Wine handlers: Wine is not installed");
            return Ok(false);
        }
        let mut options = RunOptions::default().stall(Duration::from_secs(120));
        options = options.env("WINEPREFIX", opts.prefix.display().to_string());
        options = options.env("WINEDEBUG", "-all");
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
        for (file, scheme) in defaults {
            let mut cmd = Command::new("xdg-mime");
            cmd.args(["default", file, scheme]);
            let _ = runner::run(
                cmd,
                RunOptions::default().stall(Duration::from_secs(60)).quiet(),
                |stream, line| report.line(stream, line),
            );
        }
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
/// some behind. Hide them so the menu does not offer a launcher that bypasses
/// our environment.
fn hide_wine_entries(applications: &Path, report: &mut Report<'_>) {
    let roots = [
        applications.join("wine/Programs/Image-Line"),
        applications.join("wine-protocol-flstudio.desktop"),
        applications.join("wine-protocol-fl-cloud-plugins.desktop"),
    ];
    for root in roots {
        if root.is_file() {
            hide_entry(&root, report);
        } else if root.is_dir() {
            hide_dir(&root, report);
        }
    }
}

fn hide_dir(dir: &Path, report: &mut Report<'_>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            hide_dir(&path, report);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            hide_entry(&path, report);
        }
    }
}

fn hide_entry(path: &Path, report: &mut Report<'_>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    if text.lines().any(|line| line.trim() == "NoDisplay=true") {
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

fn icons_dir() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("icons/hicolor/scalable/apps"))
}

fn mime_root() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("mime"))
}

fn mime_packages_dir() -> Result<PathBuf> {
    Ok(mime_root()?.join("packages"))
}

fn xdg_data_home() -> Result<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Ok(home_dir()?.join(".local/share")),
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        text.to_string()
    } else {
        format!("\"{}\"", text.replace('"', "\\\""))
    }
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
        assert!(text.contains("xdg-mime default"));
        assert!(!dir.path().join("FL Studio.desktop").exists());
    }

    #[test]
    fn duplicate_wine_entries_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let applications = dir.path().join("applications");
        let entry = applications.join("wine/Programs/Image-Line/FL Studio/FL Studio.desktop");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "[Desktop Entry]\nName=FL Studio\n").unwrap();
        let mut lines = Vec::new();
        let mut sink = |_, line: &str| lines.push(line.to_string());
        let mut report = Report::new(None, &mut sink);
        hide_wine_entries(&applications, &mut report);
        let text = fs::read_to_string(&entry).unwrap();
        assert!(text.contains("NoDisplay=true"));
    }

    #[test]
    fn mime_definition_covers_flp_files() {
        assert!(mime_xml().contains("*.flp"));
    }
}
