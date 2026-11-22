//! Launch FL Studio with the prefix's own Wine and a stable environment.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::http;
use crate::journal::Report;
use crate::runner::Stream;
use crate::{WINE_DIR_NAME, app_exe};

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub prefix: PathBuf,
    pub args: Vec<String>,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub target: PathBuf,
    pub args: Vec<String>,
}

pub fn launch<F>(opts: &LaunchOptions, on_line: F) -> Result<LaunchOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let wine_dir = opts.prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");
    crate::config::validate_prefix(&opts.prefix)?;

    let Some(fl_exe) = app_exe(&opts.prefix) else {
        bail!(
            "FL Studio is not installed in {}; run the installer again",
            opts.prefix.display()
        );
    };
    if !opts.dry_run && !wine.is_file() {
        bail!(
            "Wine is not installed at {}; run the setup again",
            wine.display()
        );
    }

    let envs = launch_env(&opts.prefix, &wine_dir);
    let urls = opts.args.iter().any(|arg| is_url(arg));
    let (target, args) = if urls {
        let mut args = vec!["start".to_string()];
        args.extend(opts.args.iter().cloned());
        (wine, args)
    } else {
        let args = convert_paths(&opts.prefix, &wine_dir, &opts.args);
        (fl_exe.clone(), args)
    };

    let mut cmd = Command::new(&target);
    cmd.args(&args);
    if !urls && let Some(dir) = fl_exe.parent() {
        cmd.current_dir(dir);
    }
    for (key, value) in &envs {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    if opts.dry_run {
        report.info(format!("would run {} {}", target.display(), args.join(" ")));
        for (key, value) in &envs {
            report.info(format!("env {key}={value}"));
        }
        return Ok(LaunchOutcome { target, args });
    }

    // Keep Wine's errors: they are the only clue when a plugin crashes.
    attach_log(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("starting {}", target.display()))?;
    report.info(format!(
        "launched {} (pid {})",
        target.display(),
        child.id()
    ));
    Ok(LaunchOutcome { target, args })
}

fn is_url(arg: &str) -> bool {
    arg.starts_with("flstudio://") || arg.starts_with("fl-cloud-plugins://")
}

/// Existing Unix paths are handed to FL Studio as Windows paths.
fn convert_paths(prefix: &Path, wine_dir: &Path, args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            if is_url(arg) || !Path::new(arg).exists() {
                return arg.clone();
            }
            windows_path(prefix, wine_dir, Path::new(arg)).unwrap_or_else(|| arg.clone())
        })
        .collect()
}

fn windows_path(prefix: &Path, wine_dir: &Path, path: &Path) -> Option<String> {
    let output = Command::new(wine_dir.join("bin/winepath"))
        .args(["-w"])
        .arg(path)
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", "-all")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn launch_env(prefix: &Path, wine_dir: &Path) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut envs = vec![
        ("WINEPREFIX".into(), prefix.display().to_string()),
        // Errors only: keep crash lines without drowning the log in fixmes.
        ("WINEDEBUG".into(), "-all,+err".into()),
        ("WINESERVER_TIMEOUT".into(), "60".into()),
        ("PATH".into(), format!("{}/bin:{path}", wine_dir.display())),
        (
            "WINE".into(),
            wine_dir.join("bin/wine").display().to_string(),
        ),
        (
            "WINESERVER".into(),
            wine_dir.join("bin/wineserver").display().to_string(),
        ),
        // FL Studio and its plugins are X11 clients; Wine uses XWayland on Wayland.
        (
            "DISPLAY".into(),
            std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string()),
        ),
        ("WINEDLLOVERRIDES".into(), "winemenubuilder.exe=d".into()),
        // NVIDIA blocks each OpenGL plugin's buffer swap on vblank otherwise.
        ("__GL_SYNC_TO_VBLANK".into(), "0".into()),
        ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".into(), "0".into()),
        ("__GL_SHADER_DISK_CACHE".into(), "1".into()),
    ];
    if let Ok(xauthority) = std::env::var("XAUTHORITY") {
        envs.push(("XAUTHORITY".into(), xauthority));
    }
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        // FL Studio ASIO goes through PipeWire's PulseAudio layer.
        envs.push(("PULSE_LATENCY_MSEC".into(), "20".into()));
    }
    envs
}

/// Wine writes plugin crash reports to stderr. A menu launch has no terminal,
/// so keep the last run in the cache instead.
fn attach_log(cmd: &mut Command) {
    let Ok(dir) = http::cache_dir() else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let log = dir.join("launch.log");
    match OpenOptions::new().create(true).append(true).open(&log) {
        Ok(file) => {
            let _ = file.set_len(0);
            match file.try_clone() {
                Ok(errors) => {
                    cmd.stdout(Stdio::from(file)).stderr(Stdio::from(errors));
                }
                Err(_) => {
                    cmd.stdout(Stdio::null()).stderr(Stdio::null());
                }
            }
        }
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
}

pub fn wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE")
        .map(|value| value.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_fl_urls() {
        assert!(is_url("flstudio://auth?token=x"));
        assert!(is_url("fl-cloud-plugins://login"));
        assert!(!is_url("/home/me/track.flp"));
        assert!(!is_url("https://image-line.com"));
    }

    #[test]
    fn dry_run_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
        fs::create_dir_all(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"wine").unwrap();
        let app = crate::app_dir(prefix).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();

        let opts = LaunchOptions {
            prefix: prefix.to_path_buf(),
            args: vec![],
            dry_run: true,
            log_path: None,
        };
        let mut lines = Vec::new();
        let outcome = launch(&opts, |_, line| lines.push(line.to_string())).unwrap();
        assert!(outcome.target.ends_with("FL64.exe"));
        let text = lines.join("\n");
        assert!(text.contains("would run"));
        assert!(text.contains("env WINEPREFIX="));
        assert!(text.contains("__GL_SYNC_TO_VBLANK=0"));
        assert!(text.contains("PULSE_LATENCY_MSEC=20"));
    }

    #[test]
    fn url_arguments_use_wine_start() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
        fs::create_dir_all(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"wine").unwrap();
        let app = crate::app_dir(prefix).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();

        let opts = LaunchOptions {
            prefix: prefix.to_path_buf(),
            args: vec!["flstudio://auth?code=1".into()],
            dry_run: true,
            log_path: None,
        };
        let outcome = launch(&opts, |_, _| {}).unwrap();
        assert_eq!(outcome.target, wine);
        assert_eq!(outcome.args[0], "start");
        assert_eq!(outcome.args[1], "flstudio://auth?code=1");
    }

    #[test]
    fn launch_env_keeps_errors_and_vsync_off() {
        let envs = launch_env(Path::new("/p"), Path::new("/w"));
        let has = |key: &str, value: &str| {
            envs.iter()
                .any(|(env_key, env_value)| env_key == key && env_value == value)
        };
        assert!(has("WINEDEBUG", "-all,+err"));
        assert!(has("__GL_SYNC_TO_VBLANK", "0"));
        assert!(has("WINEDLLOVERRIDES", "winemenubuilder.exe=d"));
    }
}
