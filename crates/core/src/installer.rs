//! Install FL Studio silently from Image-Line's official setup.
//!
//! The installer is NSIS and accepts `/S`. It starts Edge WebView2 near the
//! end, and Edge's services can keep the process alive after FL Studio is
//! already on disk, so a watchdog stops the prefix once FL64.exe has been
//! stable for a while.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};

use crate::http;
use crate::journal::Report;
use crate::runner::{self, RunOptions, RunResult, Stream};
use crate::{WINE_DIR_NAME, app_exe};

pub const INSTALLER_URL: &str = "https://support.image-line.com/redirect/flstudio_win_installer";
pub const INSTALLER_NAME: &str = "flstudio_win64_installer.exe";
const INSTALLER_ARGS: &[&str] = &["/S"];

#[derive(Debug, Clone)]
pub struct InstallerOptions {
    pub prefix: PathBuf,
    /// Optional user-supplied setup executable.
    pub installer: Option<PathBuf>,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct InstallerOutcome {
    pub installer: PathBuf,
    pub fl_exe: PathBuf,
    /// NSIS installers often report 199 even when they succeed.
    pub exit_code: Option<i32>,
}

pub fn install<F>(opts: &InstallerOptions, on_line: F) -> Result<InstallerOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let wine_dir = opts.prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");

    report.info(format!("FL Studio setup for {}", opts.prefix.display()));
    if !opts.dry_run && !wine.is_file() {
        bail!(
            "Wine is not installed at {}; run wine-setup first",
            wine.display()
        );
    }

    let installer = resolve_installer(opts, &mut report)?;
    if opts.dry_run {
        dry_run(opts, &installer, &wine, &wine_dir, &mut report);
        return Ok(InstallerOutcome {
            installer,
            fl_exe: app_exe(&opts.prefix)
                .unwrap_or_else(|| crate::app_dir(&opts.prefix).join("FL Studio .../FL64.exe")),
            exit_code: None,
        });
    }

    let envs = wine_env(&opts.prefix, &wine_dir);
    let before = app_exe(&opts.prefix);
    let _ = kill_prefix(&wine_dir, &envs, &mut report);

    let cancel = Arc::new(AtomicBool::new(false));
    let watchdog = before.is_none().then(|| {
        let cancel = cancel.clone();
        let prefix = opts.prefix.clone();
        let wine_dir = wine_dir.clone();
        std::thread::spawn(move || watch_new_install(&prefix, &wine_dir, &cancel))
    });

    report.info("installing silently; WebView2 can take several minutes");
    let mut cmd = installer_command(&wine, &installer);
    if let Some(parent) = installer.parent() {
        cmd.current_dir(parent);
    }
    let result = run(&mut report, cmd, &envs, Duration::from_secs(1800));

    cancel.store(true, Ordering::Relaxed);
    if let Some(watchdog) = watchdog {
        let _ = watchdog.join();
    }
    let _ = kill_prefix(&wine_dir, &envs, &mut report);
    let result = result?;

    let Some(fl_exe) = app_exe(&opts.prefix) else {
        let code = result.status.code().unwrap_or(-1);
        bail!(
            "the FL Studio installer did not create FL64.exe (exit {code}).\n{}",
            result.tail_text(20)
        );
    };

    if !result.success() {
        report.warn(format!(
            "the installer exited with {}; FL Studio is present at {}",
            result.status,
            fl_exe.display()
        ));
    }
    report.info(format!("FL Studio installed at {}", fl_exe.display()));

    Ok(InstallerOutcome {
        installer,
        fl_exe,
        exit_code: result.status.code(),
    })
}

/// Tracks whether the network download is available. Used by `install::prefetch`.
pub(crate) fn fetch_installer(report: &mut Report<'_>) -> Result<PathBuf> {
    http::download_cached(
        report,
        INSTALLER_URL,
        INSTALLER_NAME,
        Duration::from_secs(1800),
    )
}

fn resolve_installer(opts: &InstallerOptions, report: &mut Report<'_>) -> Result<PathBuf> {
    let Some(source) = &opts.installer else {
        if opts.dry_run {
            let cached = http::cache_dir()?.join(INSTALLER_NAME);
            report.info(format!(
                "    curl -fLR -z {0} -o {0} {INSTALLER_URL}",
                cached.display()
            ));
            return Ok(cached);
        }
        return fetch_installer(report);
    };

    if opts.dry_run {
        report.info(format!("    use local installer {}", source.display()));
        return Ok(source.clone());
    }
    if !source.is_file() {
        bail!("installer not found: {}", source.display());
    }
    Ok(source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf()))
}

fn installer_command(wine: &Path, installer: &Path) -> Command {
    let mut cmd = Command::new(wine);
    cmd.arg(installer);
    cmd.args(INSTALLER_ARGS);
    cmd
}

fn dry_run(
    opts: &InstallerOptions,
    installer: &Path,
    wine: &Path,
    wine_dir: &Path,
    report: &mut Report<'_>,
) {
    report.info(format!(
        "    WINEPREFIX={} {} {} {}",
        opts.prefix.display(),
        wine.display(),
        shell_quote(&installer.display().to_string()),
        INSTALLER_ARGS.join(" ")
    ));
    report.info(format!(
        "    wait for {}",
        crate::app_dir(&opts.prefix)
            .join("FL Studio <year>/FL64.exe")
            .display()
    ));
    report.info(format!(
        "    {} -k",
        wine_dir.join("bin/wineserver").display()
    ));
    report.info("    the installer may report exit code 199 after success");
}

/// Stop the prefix once FL64.exe exists and has not changed for a minute.
/// The installer starts Edge services that otherwise keep it alive forever.
fn watch_new_install(prefix: &Path, wine_dir: &Path, cancel: &AtomicBool) {
    let started = Instant::now();
    let mut stable: Option<(u64, Option<SystemTime>, Instant)> = None;
    while !cancel.load(Ordering::Relaxed) {
        if started.elapsed() > Duration::from_secs(45 * 60) {
            return;
        }
        if let Some(exe) = app_exe(prefix)
            && let Ok(metadata) = fs::metadata(&exe)
        {
            let len = metadata.len();
            let modified = metadata.modified().ok();
            stable = match stable {
                Some((last_len, last_modified, since))
                    if last_len == len && last_modified == modified =>
                {
                    // WebView2 still needs to finish after FL64.exe appears;
                    // wait longer until its runtime shows up.
                    let grace = if crate::components::webview2_installed(prefix) {
                        Duration::from_secs(120)
                    } else {
                        Duration::from_secs(600)
                    };
                    if since.elapsed() >= grace {
                        stop_prefix(prefix, wine_dir);
                        return;
                    }
                    Some((last_len, last_modified, since))
                }
                _ => Some((len, modified, Instant::now())),
            };
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn stop_prefix(prefix: &Path, wine_dir: &Path) {
    let wineserver = wine_dir.join("bin/wineserver");
    if !wineserver.is_file() {
        return;
    }
    let _ = Command::new(wineserver)
        .arg("-k")
        .env("WINEPREFIX", prefix)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn kill_prefix(wine_dir: &Path, envs: &[(String, String)], report: &mut Report<'_>) -> Result<()> {
    let mut cmd = Command::new(wine_dir.join("bin/wineserver"));
    cmd.arg("-k");
    let _ = run(report, cmd, envs, Duration::from_secs(30));
    Ok(())
}

fn run(
    report: &mut Report<'_>,
    cmd: Command,
    envs: &[(String, String)],
    stall: Duration,
) -> Result<RunResult> {
    let mut options = RunOptions::default().stall(stall);
    for (key, value) in envs {
        options = options.env(key.clone(), value.clone());
    }
    runner::run(cmd, options, |stream, line| report.line(stream, line))
}

fn wine_env(prefix: &Path, wine_dir: &Path) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_default();
    vec![
        ("WINEPREFIX".into(), prefix.display().to_string()),
        ("WINEDEBUG".into(), "-all".into()),
        ("WINESERVER_TIMEOUT".into(), "60".into()),
        (
            "WINEDLLOVERRIDES".into(),
            "winemenubuilder.exe=d;winegstreamer=".into(),
        ),
        (
            "PATH".into(),
            format!("{}:{path}", wine_dir.join("bin").display()),
        ),
        (
            "WINE".into(),
            wine_dir.join("bin/wine").display().to_string(),
        ),
        (
            "WINESERVER".into(),
            wine_dir.join("bin/wineserver").display().to_string(),
        ),
    ]
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
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

    fn options(prefix: &Path) -> InstallerOptions {
        InstallerOptions {
            prefix: prefix.to_path_buf(),
            installer: None,
            dry_run: true,
            log_path: None,
        }
    }

    #[test]
    fn dry_run_prints_the_silent_install() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        install(&options(dir.path()), |_, line| lines.push(line.to_string())).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("flstudio_win64_installer.exe"));
        assert!(text.contains("/S"));
        assert!(text.contains("FL64.exe"));
        assert!(text.contains("wineserver"));
        assert!(text.contains("199"));
        assert!(!dir.path().join("Installer").exists());
    }

    #[test]
    fn local_installer_is_used_directly() {
        let dir = tempfile::tempdir().unwrap();
        let installer = dir.path().join("flstudio_win64_26.1.7.5654.exe");
        fs::write(&installer, b"MZ").unwrap();
        let opts = InstallerOptions {
            prefix: dir.path().join("prefix"),
            installer: Some(installer.clone()),
            dry_run: true,
            log_path: None,
        };
        let mut lines = Vec::new();
        install(&opts, |_, line| lines.push(line.to_string())).unwrap();
        assert!(lines.join("\n").contains(&installer.display().to_string()));
    }

    #[test]
    fn silent_flag_follows_the_installer() {
        let cmd = installer_command(
            Path::new("/wine/bin/wine"),
            Path::new("/tmp/flstudio_win64.exe"),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["/tmp/flstudio_win64.exe", "/S"]);
    }

    #[test]
    fn missing_local_installer_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = InstallerOptions {
            prefix: dir.path().to_path_buf(),
            installer: Some(dir.path().join("missing.exe")),
            dry_run: false,
            log_path: None,
        };
        assert!(install(&opts, |_, _| {}).is_err());
    }
}
