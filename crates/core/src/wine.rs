//! Private Wine bootstrap for the FL Studio prefix.
//!
//! FL Studio 26.1.4 and later refuse to start on Wine before 11.21, which
//! fixed the crypt32 signature check (WineHQ bug 60273). The prefix therefore
//! always uses a Chromium-free upstream build from Kron4ek, not the distro's
//! Wine, and records that build inside the prefix.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::http;
use crate::journal::Report;
use crate::runner::{self, RunOptions, RunResult, Stream};
use crate::{WINE_DIR_NAME, deps};

pub const WINE_VERSION: &str = "11.22";
pub const WINE_FALLBACK_VERSION: &str = "11.21";

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub prefix: PathBuf,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
    /// Force a specific build. Switching inside an initialized prefix is refused.
    pub wine_version: Option<String>,
    /// Wine DPI (LogPixels) to write; `None` leaves the prefix's DPI alone.
    pub dpi: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct SetupReport {
    pub wine_version: Option<String>,
    pub wine_dir: Option<PathBuf>,
    pub initialized: bool,
}

/// Download, link and configure the Wine build for `opts.prefix`.
pub fn setup<F>(opts: &SetupOptions, on_line: F) -> Result<SetupReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let prefix = &opts.prefix;

    crate::config::validate_prefix(prefix)?;
    if !opts.dry_run
        && prefix.exists()
        && !crate::config::looks_like_prefix(prefix)
        && fs::read_dir(prefix)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name() != std::ffi::OsStr::new("logs"))
            })
            .unwrap_or(false)
    {
        bail!(
            "{} is not empty and does not look like a Wine prefix; choose another folder",
            prefix.display()
        );
    }
    report.info(format!("Wine setup for {}", prefix.display()));

    if !opts.dry_run {
        fs::create_dir_all(prefix)
            .with_context(|| format!("creating prefix {}", prefix.display()))?;
    }

    let wine_version = ensure_wine(
        prefix,
        opts.wine_version.as_deref(),
        opts.dry_run,
        &mut report,
    )?;
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let mut initialized = false;

    if prefix.join("system.reg").exists() {
        report.info("prefix is already initialized; keeping its Wine environment");
        initialized = true;
    } else if opts.dry_run {
        report.info(format!(
            "    WINEPREFIX={} WINEARCH=win64 {} -u",
            prefix.display(),
            wine_dir.join("bin/wineboot").display()
        ));
    } else {
        report.info("initializing the Wine prefix");
        kill_prefix(prefix, Some(&wine_dir), &mut report)?;
        let mut cmd = Command::new(wine_dir.join("bin/wineboot"));
        cmd.arg("-u");
        let mut run_opts = stream_opts(Some(Duration::from_secs(900)));
        run_opts.envs = wine_env(prefix, &wine_dir);
        // Gecko, Mono and winegstreamer are not needed for FL Studio and can
        // hang a fresh wineboot on systems with gst-editing-services.
        run_opts.envs.push(("WINEARCH".into(), "win64".into()));
        run_opts.envs.push((
            "WINEDLLOVERRIDES".into(),
            "mscoree=;mshtml=;winemenubuilder.exe=d;winegstreamer=".into(),
        ));
        let result = run_logged(&mut report, cmd, run_opts)?;
        if !result.success() {
            bail!("wineboot failed\n{}", result.tail_text(20));
        }
        if !prefix.join("drive_c").is_dir() {
            bail!("prefix initialization did not create drive_c");
        }
        initialized = true;
    }

    configure(prefix, opts.dpi, opts.dry_run, &mut report)?;

    Ok(SetupReport {
        wine_version: Some(wine_version),
        wine_dir: Some(wine_dir),
        initialized,
    })
}

/// Re-apply the Windows version and DPI after winetricks, without touching
/// the WebView2 keys.
pub fn reapply_windows<F>(
    prefix: &Path,
    dpi: Option<u32>,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let wine_dir = prefix.join(WINE_DIR_NAME);
    set_windows_11(prefix, &wine_dir, dry_run, &mut report)?;
    if let Some(dpi) = dpi {
        set_dpi(prefix, &wine_dir, dpi, dry_run, &mut report)?;
    }
    Ok(())
}

/// Re-apply the WebView2 keys after FL Studio's installer added the runtime.
pub fn configure_webview2_prefix<F>(
    prefix: &Path,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let wine_dir = prefix.join(WINE_DIR_NAME);
    configure_webview2(prefix, &wine_dir, dry_run, &mut report)
}

fn configure(
    prefix: &Path,
    dpi: Option<u32>,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let wine_dir = prefix.join(WINE_DIR_NAME);
    set_windows_11(prefix, &wine_dir, dry_run, report)?;
    configure_webview2(prefix, &wine_dir, dry_run, report)?;
    if let Some(dpi) = dpi {
        set_dpi(prefix, &wine_dir, dpi, dry_run, report)?;
    }
    Ok(())
}

fn set_windows_11(
    prefix: &Path,
    wine_dir: &Path,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let winecfg = wine_dir.join("bin/winecfg");
    report.info("setting Windows version to 11");
    if dry_run {
        report.info(format!("    {} -v win11", winecfg.display()));
        return Ok(());
    }
    if !winecfg.is_file() {
        report.warn(format!("{} is missing", winecfg.display()));
        return Ok(());
    }
    kill_prefix(prefix, Some(wine_dir), report)?;
    let mut cmd = Command::new(&winecfg);
    cmd.args(["-v", "win11"]);
    let mut run_opts = stream_opts(Some(Duration::from_secs(300)));
    run_opts.envs = wine_env(prefix, wine_dir);
    let result = run_logged(report, cmd, run_opts)?;
    if !result.success() {
        report.warn("winecfg failed to set Windows 11; continuing");
    }
    Ok(())
}

/// WebView2's Chromium compositor needs DirectComposition, which Wine does not
/// implement. Reporting Windows 8 to the runtime alone makes it fall back to
/// software compositing instead of showing black panes. The Edge updater is
/// disabled so it cannot replace the runtime behind the prefix's back.
fn configure_webview2(
    prefix: &Path,
    wine_dir: &Path,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    report.info("configuring WebView2 and its updater");
    let entries = [
        (
            "HKCU\\Software\\Wine\\AppDefaults\\msedgewebview2.exe",
            "Version",
            "REG_SZ",
            "win8",
        ),
        (
            "HKLM\\System\\CurrentControlSet\\Services\\edgeupdate",
            "Start",
            "REG_DWORD",
            "4",
        ),
        (
            "HKLM\\System\\CurrentControlSet\\Services\\edgeupdatem",
            "Start",
            "REG_DWORD",
            "4",
        ),
        (
            "HKLM\\Software\\Policies\\Microsoft\\EdgeUpdate",
            "UpdateDefault",
            "REG_DWORD",
            "0",
        ),
    ];
    for (key, name, kind, value) in entries {
        reg_add(prefix, wine_dir, key, name, kind, value, dry_run, report)?;
    }
    Ok(())
}

/// Write the Wine DPI into an existing prefix, e.g. during a repair.
pub fn apply_dpi<F>(
    prefix: &Path,
    dpi: u32,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    set_dpi(
        prefix,
        &prefix.join(WINE_DIR_NAME),
        dpi,
        dry_run,
        &mut report,
    )
}

fn set_dpi(
    prefix: &Path,
    wine_dir: &Path,
    dpi: u32,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    report.info(format!(
        "setting Wine DPI to {dpi} ({}%)",
        (dpi * 100 + 48) / 96
    ));
    reg_add(
        prefix,
        wine_dir,
        "HKCU\\Control Panel\\Desktop",
        "LogPixels",
        "REG_DWORD",
        &dpi.to_string(),
        dry_run,
        report,
    )?;
    reg_add(
        prefix,
        wine_dir,
        "HKCU\\Software\\Wine\\Fonts",
        "LogPixels",
        "REG_DWORD",
        &dpi.to_string(),
        dry_run,
        report,
    )
}

#[allow(clippy::too_many_arguments)]
fn reg_add(
    prefix: &Path,
    wine_dir: &Path,
    key: &str,
    name: &str,
    kind: &str,
    value: &str,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let mut cmd = Command::new(wine_dir.join("bin/wine"));
    cmd.args(["reg", "add", key, "/v", name, "/t", kind, "/d", value, "/f"]);
    if dry_run {
        report.info(format!(
            "    WINEPREFIX={} {}",
            prefix.display(),
            describe(&cmd)
        ));
        return Ok(());
    }
    let mut run_opts = stream_opts(Some(Duration::from_secs(120)));
    run_opts.envs = wine_env(prefix, wine_dir);
    let result = run_logged(report, cmd, run_opts)?;
    if !result.success() {
        report.warn(format!("could not set {key}\\{name}; continuing"));
    }
    Ok(())
}

fn ensure_wine(
    prefix: &Path,
    requested: Option<&str>,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<String> {
    if let Some(existing) = detect_installed_build(prefix) {
        let usable = supports_fl(&existing);
        let compatible = requested
            .map(|requested| existing.starts_with(requested))
            .unwrap_or(true);
        if usable && compatible {
            report.info(format!("using existing Wine {existing}"));
            return Ok(existing);
        }

        if prefix.join("system.reg").exists() {
            if !usable {
                bail!(
                    "prefix uses Wine {existing}, but FL Studio 26.1.4 and later need Wine 11.21 or newer. Remove {} and rerun to build a new prefix.",
                    prefix.display()
                );
            }
            bail!(
                "prefix already uses Wine {existing}; refusing to switch builds inside an initialized prefix. Remove {} and rerun with the requested version.",
                prefix.display()
            );
        }
        report.warn(format!(
            "replacing Wine {existing} with {} in an uninitialized prefix",
            requested.unwrap_or(WINE_VERSION)
        ));
    }

    install_wine(prefix, requested, dry_run, report)
}

/// The Wine build recorded by the `wine-fl` symlink.
pub fn detect_installed_build(prefix: &Path) -> Option<String> {
    let link = prefix.join(WINE_DIR_NAME);
    if !link.join("bin/wine").is_file() {
        return None;
    }
    if let Ok(target) = fs::read_link(&link)
        && let Some(name) = target.file_name().and_then(|name| name.to_str())
        && let Some(version) = name
            .strip_prefix("wine-")
            .and_then(|name| name.strip_suffix("-amd64"))
    {
        return Some(version.to_string());
    }
    Some("custom".to_string())
}

/// FL Studio 26.1.4 and later require the crypt32 fix from Wine 11.21.
fn supports_fl(version: &str) -> bool {
    if version == "custom" {
        return true;
    }
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => major > 11 || (major == 11 && minor >= 21),
        (Some(major), None) => major > 11,
        _ => false,
    }
}

fn install_wine(
    prefix: &Path,
    requested: Option<&str>,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<String> {
    let candidates = match requested {
        Some(version) => {
            release_for(version)?;
            vec![version.to_string()]
        }
        None => vec![WINE_VERSION.to_string(), WINE_FALLBACK_VERSION.to_string()],
    };

    if let Some(cached) = candidates
        .iter()
        .find(|version| find_wine_dir(prefix, version).is_some())
    {
        let dir = find_wine_dir(prefix, cached).expect("checked above");
        report.info(format!("found extracted Wine {cached}"));
        if dry_run {
            report.info(format!(
                "    ln -s {} {}",
                dir.display(),
                prefix.join(WINE_DIR_NAME).display()
            ));
        } else {
            link_wine(prefix, &dir)?;
            verify_wine(prefix, dry_run, report)?;
        }
        return Ok(cached.clone());
    }

    if dry_run {
        let release = release_for(&candidates[0])?;
        let archive = http::cache_dir()?.join(&release.file_name);
        report.info(format!(
            "    curl -fLR -z {0} -o {0} {1}",
            archive.display(),
            release.url
        ));
        report.info(format!(
            "    echo '{}  {}' | sha256sum -c -",
            release.sha256,
            archive.display()
        ));
        report.info(format!(
            "    tar -xJf {} -C {} --no-same-owner",
            archive.display(),
            prefix.display()
        ));
        report.info(format!(
            "    ln -s {} {}",
            release.dir_pattern,
            prefix.join(WINE_DIR_NAME).display()
        ));
        report.info(format!(
            "    {} --version",
            prefix.join(WINE_DIR_NAME).join("bin/wine").display()
        ));
        if candidates.len() > 1 {
            report.info(format!(
                "    fallback: Wine {} if {} fails",
                candidates[1], candidates[0]
            ));
        }
        return Ok(candidates[0].clone());
    }

    let mut errors = Vec::new();
    for version in &candidates {
        let release = release_for(version)?;

        if let Some(dir) = find_wine_dir(prefix, version) {
            report.info(format!("found extracted Wine {version}"));
            link_wine(prefix, &dir)?;
            verify_wine(prefix, dry_run, report)?;
            return Ok(version.clone());
        }

        report.info(format!("downloading Wine {version}"));
        let archive = match fetch_archive(report, &release) {
            Ok(archive) => archive,
            Err(error) => {
                report.warn(format!("Wine {version} download failed: {error:#}"));
                errors.push(format!("{version}: download failed"));
                continue;
            }
        };

        report.info(format!("extracting Wine {version}"));
        let mut cmd = Command::new("tar");
        cmd.args(["-xJf"])
            .arg(&archive)
            .arg("-C")
            .arg(prefix)
            .args(["--no-same-owner", "--overwrite"]);
        let run_opts = stream_opts(Some(Duration::from_secs(1800)));
        let result = run_logged(report, cmd, run_opts)?;
        if !result.success() {
            let _ = fs::remove_file(&archive);
            report.warn(format!(
                "Wine {version} extraction failed\n{}",
                result.tail_text(10)
            ));
            errors.push(format!("{version}: extraction failed"));
            continue;
        }

        let Some(dir) = find_wine_dir(prefix, version) else {
            report.warn(format!(
                "Wine {version} archive did not contain {}",
                release.dir_pattern
            ));
            errors.push(format!("{version}: archive layout not recognized"));
            continue;
        };

        link_wine(prefix, &dir)?;
        verify_wine(prefix, dry_run, report)?;
        return Ok(version.clone());
    }

    bail!(
        "could not install a working Wine build ({})",
        errors.join("; ")
    )
}

fn fetch_archive(report: &mut Report<'_>, release: &Release) -> Result<PathBuf> {
    let archive = http::download_cached(
        report,
        &release.url,
        &release.file_name,
        Duration::from_secs(600),
    )?;
    let hash = http::sha256_file(&archive)?;
    if hash == release.sha256 {
        return Ok(archive);
    }
    report.warn(format!(
        "{} failed its SHA-256 check; downloading it again",
        release.file_name
    ));
    let _ = fs::remove_file(&archive);
    http::download(report, &release.url, &archive, Duration::from_secs(600))?;
    if http::sha256_file(&archive)? != release.sha256 {
        let _ = fs::remove_file(&archive);
        bail!("the Wine archive failed its SHA-256 check twice");
    }
    Ok(archive)
}

fn release_for(version: &str) -> Result<Release> {
    let (sha256, dir_pattern) = match version {
        "11.22" => (
            "cdaaa76c218359d486eb35968892a4c466623b137ed0931ee3f31465ec6539e0",
            "wine-11.22-amd64",
        ),
        "11.21" => (
            "1f12f2aef4f5d0b5c0548ceecdb20068edc59fcd23a5da481e7059a25070eac9",
            "wine-11.21-amd64",
        ),
        other => bail!(
            "unsupported Wine version '{other}' (expected {WINE_VERSION} or {WINE_FALLBACK_VERSION})"
        ),
    };
    Ok(Release {
        dir_pattern: dir_pattern.to_string(),
        file_name: format!("{dir_pattern}.tar.xz"),
        url: format!(
            "https://github.com/Kron4ek/Wine-Builds/releases/download/{version}/{dir_pattern}.tar.xz"
        ),
        sha256: sha256.to_string(),
    })
}

struct Release {
    dir_pattern: String,
    file_name: String,
    url: String,
    sha256: String,
}

fn find_wine_dir(prefix: &Path, version: &str) -> Option<PathBuf> {
    let pattern = format!("wine-{version}-amd64");
    let entries = fs::read_dir(prefix).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && entry.file_name().to_string_lossy().starts_with(&pattern)
            && path.join("bin/wine").is_file()
        {
            return Some(path);
        }
    }
    None
}

fn link_wine(prefix: &Path, extracted: &Path) -> Result<()> {
    let link = prefix.join(WINE_DIR_NAME);
    if let Ok(metadata) = link.symlink_metadata() {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(&link)
                .with_context(|| format!("removing old Wine link {}", link.display()))?;
        } else if metadata.is_dir() {
            if link.join("bin/wine").is_file() {
                return Ok(());
            }
            bail!(
                "{} exists and is not a usable Wine build; remove it manually",
                link.display()
            );
        }
    }

    let target = extracted.strip_prefix(prefix).unwrap_or(extracted);
    std::os::unix::fs::symlink(target, &link)
        .with_context(|| format!("linking {} to {}", link.display(), target.display()))
}

fn verify_wine(prefix: &Path, dry_run: bool, report: &mut Report<'_>) -> Result<()> {
    let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
    if !wine.is_file() {
        bail!("Wine binary is missing at {}", wine.display());
    }
    if dry_run {
        report.info(format!("    {} --version", wine.display()));
        return Ok(());
    }

    let mut cmd = Command::new(&wine);
    cmd.arg("--version");
    let mut run_opts = stream_opts(Some(Duration::from_secs(120)));
    run_opts.envs = wine_env(prefix, &prefix.join(WINE_DIR_NAME));
    let result = run_logged(report, cmd, run_opts)?;
    if !result.success() {
        bail!(
            "Wine binary at {} is not functional\n{}",
            wine.display(),
            result.tail_text(10)
        );
    }
    Ok(())
}

fn kill_prefix(prefix: &Path, wine_dir: Option<&Path>, report: &mut Report<'_>) -> Result<()> {
    let wineserver = wine_dir
        .map(|dir| dir.join("bin/wineserver"))
        .filter(|path| path.is_file())
        .or_else(|| deps::which("wineserver"));
    let Some(wineserver) = wineserver else {
        return Ok(());
    };

    let mut cmd = Command::new(wineserver);
    cmd.arg("-k");
    let mut run_opts = stream_opts(Some(Duration::from_secs(30)));
    run_opts.echo = false;
    run_opts
        .envs
        .push(("WINEPREFIX".into(), prefix.display().to_string()));
    let _ = run_logged(report, cmd, run_opts)?;
    Ok(())
}

fn wine_env(prefix: &Path, wine_dir: &Path) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_default();
    vec![
        ("WINEPREFIX".into(), prefix.display().to_string()),
        ("WINEDEBUG".into(), "-all,fixme-all".into()),
        ("WINESERVER_TIMEOUT".into(), "60".into()),
        ("WINETRICKS_GUI".into(), "none".into()),
        (
            "PATH".into(),
            format!("{}:{path}", wine_dir.join("bin").display()),
        ),
    ]
}

/// How the X server Wine draws on relates to the real display. Wine always
/// runs under X11 here, which on Wayland means XWayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XScaling {
    /// X11 clients get physical pixels: X11 sessions, KDE's default.
    Physical,
    /// GNOME's default: mutter renders X11 clients at 1x and upscales them.
    Upscaled,
    /// GNOME with `xwayland-native-scaling`: X11 clients get a canvas at the
    /// scale rounded up (2x at 125%), which mutter then shrinks.
    RoundedUp,
}

impl XScaling {
    pub fn label(&self) -> &'static str {
        match self {
            XScaling::Physical => "physical pixels",
            XScaling::Upscaled => "GNOME upscales X11 apps",
            XScaling::RoundedUp => "GNOME XWayland native scaling",
        }
    }
}

pub fn detect_x_scaling() -> XScaling {
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktop| desktop.to_ascii_lowercase().contains("gnome"))
        .unwrap_or(false);
    if !gnome || !crate::launch::wayland_session() {
        return XScaling::Physical;
    }
    let features = Command::new("gsettings")
        .args(["get", "org.gnome.mutter", "experimental-features"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if has_native_xwayland_scaling(&features) {
        XScaling::RoundedUp
    } else {
        XScaling::Upscaled
    }
}

fn has_native_xwayland_scaling(features: &str) -> bool {
    features.contains("'xwayland-native-scaling'")
}

/// Wine DPI for a display, in the 12-DPI steps of Wine's scaling slider.
/// With physical pixels it is the larger of a resolution-based value and the
/// host scale. `resolution` is the monitor's physical size; without it only
/// the scale counts.
pub fn recommended_dpi(resolution: Option<(u32, u32)>, scale: f32, x: XScaling) -> u32 {
    match x {
        XScaling::Physical => {}
        XScaling::Upscaled => return 96,
        XScaling::RoundedUp => return (96 * scale.ceil().max(1.0) as u32).min(480),
    }
    let by_resolution = match resolution {
        Some((width, height)) if width >= 3840 || height >= 2160 => 192,
        Some((width, height)) if width >= 2560 || height >= 1440 => 144,
        _ => 96,
    };
    let steps = (96.0 * f64::from(scale) / 12.0).round() as u32;
    let by_scale = (steps * 12).clamp(96, 480);
    by_resolution.max(by_scale)
}

fn stream_opts(stall: Option<Duration>) -> RunOptions {
    let mut options = RunOptions::default();
    if let Some(stall) = stall {
        options = options.stall(stall);
    }
    options
}

fn run_logged(report: &mut Report<'_>, cmd: Command, opts: RunOptions) -> Result<RunResult> {
    runner::run(cmd, opts, |stream, line| report.line(stream, line))
}

fn describe(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().to_string()];
    for arg in cmd.get_args() {
        parts.push(shell_quote(&arg.to_string_lossy()));
    }
    parts.join(" ")
}

fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=@+".contains(c))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(prefix: &Path) -> SetupOptions {
        SetupOptions {
            prefix: prefix.to_path_buf(),
            dry_run: true,
            log_path: None,
            wine_version: None,
            dpi: None,
        }
    }

    #[test]
    fn dpi_follows_resolution_and_scale() {
        let physical = XScaling::Physical;
        assert_eq!(recommended_dpi(Some((3840, 2400)), 2.25, physical), 216);
        assert_eq!(recommended_dpi(Some((3840, 2160)), 1.0, physical), 192);
        assert_eq!(recommended_dpi(Some((2560, 1440)), 1.25, physical), 144);
        assert_eq!(recommended_dpi(Some((1920, 1080)), 1.0, physical), 96);
        assert_eq!(recommended_dpi(None, 1.5, physical), 144);
        assert_eq!(recommended_dpi(None, 0.5, physical), 96);
        assert_eq!(recommended_dpi(None, 8.0, physical), 480);
    }

    #[test]
    fn dpi_on_gnome_follows_how_mutter_scales_xwayland() {
        assert_eq!(
            recommended_dpi(Some((3840, 2400)), 2.0, XScaling::Upscaled),
            96
        );
        assert_eq!(
            recommended_dpi(Some((2560, 1600)), 1.25, XScaling::RoundedUp),
            192
        );
        assert_eq!(recommended_dpi(None, 1.0, XScaling::RoundedUp), 96);
        assert_eq!(recommended_dpi(None, 2.5, XScaling::RoundedUp), 288);
    }

    #[test]
    fn reads_mutter_experimental_features() {
        assert!(has_native_xwayland_scaling(
            "['scale-monitor-framebuffer', 'xwayland-native-scaling']"
        ));
        assert!(!has_native_xwayland_scaling(
            "['scale-monitor-framebuffer']"
        ));
        assert!(!has_native_xwayland_scaling("@as []"));
    }

    #[test]
    fn release_assets_match_upstream() {
        let latest = release_for("11.22").unwrap();
        assert_eq!(latest.file_name, "wine-11.22-amd64.tar.xz");
        assert_eq!(latest.dir_pattern, "wine-11.22-amd64");
        assert!(latest.url.contains("/releases/download/11.22/"));
        assert_eq!(latest.sha256.len(), 64);
        assert!(release_for("11.20").is_err());
    }

    #[test]
    fn dry_run_does_not_touch_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        let mut lines = Vec::new();

        let report = setup(&options(&prefix), |_, line| lines.push(line.to_string())).unwrap();

        assert!(!prefix.exists());
        assert_eq!(report.wine_version.as_deref(), Some("11.22"));
        let text = lines.join("\n");
        assert!(text.contains("wine-11.22-amd64.tar.xz"));
        assert!(text.contains("wineboot"));
        assert!(text.contains("winecfg"));
        assert!(text.contains("msedgewebview2.exe"));
        assert!(text.contains("edgeupdate"));
    }

    #[test]
    fn dpi_is_written_to_both_keys() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
        fs::create_dir_all(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"wine").unwrap();
        let mut lines = Vec::new();
        apply_dpi(prefix, 144, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("LogPixels"));
        assert!(text.contains("HKCU\\Control Panel\\Desktop"));
        assert!(text.contains("HKCU\\Software\\Wine\\Fonts"));
        assert!(text.contains("144"));
    }

    #[test]
    fn detects_build_from_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let build = prefix.join("wine-11.22-amd64/bin");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("wine"), "fake").unwrap();
        std::os::unix::fs::symlink("wine-11.22-amd64", prefix.join(WINE_DIR_NAME)).unwrap();

        assert_eq!(detect_installed_build(prefix).as_deref(), Some("11.22"));
    }

    #[test]
    fn detects_extracted_build() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let build = prefix.join("wine-11.21-amd64/bin");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("wine"), "fake").unwrap();

        assert!(find_wine_dir(prefix, "11.21").is_some());
        assert!(find_wine_dir(prefix, "11.22").is_none());
    }

    #[test]
    fn enforces_the_fl_studio_minimum() {
        assert!(supports_fl("11.21"));
        assert!(supports_fl("11.22"));
        assert!(supports_fl("12.0"));
        assert!(!supports_fl("11.20"));
        assert!(!supports_fl("11.9"));
        assert!(!supports_fl("10.99"));
        assert!(supports_fl("custom"));
    }
}
