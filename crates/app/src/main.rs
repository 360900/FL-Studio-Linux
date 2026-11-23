slint::include_modules!();

mod integration;

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use fl_studio_linux_core::config::{Config, default_prefix, validate_prefix};
use fl_studio_linux_core::deps;
use fl_studio_linux_core::doctor::{self, DoctorReport};
use fl_studio_linux_core::gpu::{self, Vendor};
use fl_studio_linux_core::install::{self, SetupOptions};
use fl_studio_linux_core::launch;
use fl_studio_linux_core::manifest::now_epoch;
use fl_studio_linux_core::packages::{self, Elevation};
use fl_studio_linux_core::runner::Stream;
use fl_studio_linux_core::uninstall;
use fl_studio_linux_core::wine;
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, LogicalSize, Model, VecModel};

enum UiEvent {
    Doctor(Box<DoctorReport>),
    DoctorFailed(String),
    Log {
        text: String,
        error: bool,
        status: bool,
    },
    JobDone(&'static str, Vec<String>),
    JobFailed(&'static str, String),
}

fn main() -> Result<(), slint::PlatformError> {
    integration::register();
    let ui = MainWindow::new()?;
    // Matches the desktop file name, so Wayland compositors show its icon.
    slint::set_xdg_app_id(integration::APP_ID)?;
    let cfg = Config::load().unwrap_or_else(|err| {
        eprintln!("config: {err:#}");
        Config::default()
    });

    let logs: Rc<VecModel<LogLine>> = Rc::new(VecModel::from(Vec::new()));
    ui.set_logs(logs.clone().into());
    ui.set_prefix_text(display_path(&cfg.prefix).into());
    if let Some(family) = monospace_family() {
        ui.set_mono_font(family.into());
    }
    ui.set_busy(true);
    ui.set_status_text("Checking your system…".into());

    let (tx, rx) = mpsc::channel::<UiEvent>();
    spawn_doctor(cfg.clone(), tx.clone());
    // A job is running, as opposed to the startup check, which may be closed.
    let job_running = Rc::new(Cell::new(false));
    // The user tried to close during a job; keep saying why it stays open.
    let close_blocked = Rc::new(Cell::new(false));
    // The prefix the window shows, which the uninstall dialog names.
    let shown_prefix = Rc::new(RefCell::new(cfg.prefix.clone()));

    {
        let weak = ui.as_weak();
        let cfg = cfg.clone();
        let tx = tx.clone();
        let job_running = job_running.clone();
        ui.on_install_clicked(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let cfg = Config::load().unwrap_or_else(|_| cfg.clone());
            let display = display_info(&ui);
            start_job(&ui, &job_running, "Installing…");
            spawn_install(cfg, display, tx.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let cfg = cfg.clone();
        let tx = tx.clone();
        let job_running = job_running.clone();
        ui.on_launch_clicked(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let cfg = Config::load().unwrap_or_else(|_| cfg.clone());
            start_job(&ui, &job_running, "Opening…");
            spawn_launch(cfg, tx.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let cfg = cfg.clone();
        let tx = tx.clone();
        let job_running = job_running.clone();
        ui.on_update_clicked(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let cfg = Config::load().unwrap_or_else(|_| cfg.clone());
            let display = display_info(&ui);
            start_job(&ui, &job_running, "Updating…");
            spawn_update(cfg, display, tx.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let cfg = cfg.clone();
        let tx = tx.clone();
        let job_running = job_running.clone();
        ui.on_repair_clicked(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let cfg = Config::load().unwrap_or_else(|_| cfg.clone());
            let display = display_info(&ui);
            start_job(&ui, &job_running, "Repairing…");
            spawn_repair(cfg, display, tx.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let cfg = cfg.clone();
        let tx = tx.clone();
        let job_running = job_running.clone();
        let shown_prefix = shown_prefix.clone();
        ui.on_uninstall_clicked(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_busy() {
                return;
            }
            let cfg = Config::load().unwrap_or_else(|_| cfg.clone());
            // Delete the folder the dialog named, even if the config changed
            // since the window last checked.
            let prefix = shown_prefix.borrow().clone();
            start_job(&ui, &job_running, "Uninstalling…");
            spawn_uninstall(cfg, prefix, tx.clone());
        });
    }
    {
        let weak = ui.as_weak();
        let job_running = job_running.clone();
        let close_blocked = close_blocked.clone();
        ui.window().on_close_requested(move || {
            // Closing mid-job would kill it halfway, e.g. during a package
            // install or while the prefix is being deleted.
            if !job_running.get() {
                return slint::CloseRequestResponse::HideWindow;
            }
            close_blocked.set(true);
            if let Some(ui) = weak.upgrade() {
                ui.set_status_text("Wait for this to finish, then close the window".into());
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_details_toggled(move || {
            // Measure on the next turn: the log panel is still in the layout
            // while this callback runs, so fit-height would be stale.
            let weak = weak.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = weak.upgrade() {
                    fit_window(&ui);
                }
            });
        });
    }

    let weak = ui.as_weak();
    let logs = logs.clone();
    let tx = tx.clone();
    let mut checking = true;
    // Set after a job so the follow-up system check keeps its result message.
    let mut keep_state = false;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            while let Ok(event) = rx.try_recv() {
                match event {
                    UiEvent::Doctor(report) => {
                        if checking {
                            checking = false;
                            ui.set_busy(false);
                        }
                        *shown_prefix.borrow_mut() = report.prefix.clone();
                        apply_report(&ui, &logs, *report, keep_state);
                        keep_state = false;
                        fit_window_later(&ui);
                    }
                    UiEvent::DoctorFailed(message) => {
                        if checking {
                            checking = false;
                            ui.set_busy(false);
                        }
                        ui.set_error_text(format!("System check failed: {message}").into());
                        push_log(&logs, format!("error: system check: {message}"), true);
                        fit_window_later(&ui);
                    }
                    UiEvent::Log {
                        text,
                        error,
                        status,
                    } => {
                        if status && !error && !close_blocked.get() {
                            ui.set_status_text(text.clone().into());
                        }
                        push_log(&logs, text, error);
                    }
                    UiEvent::JobDone(kind, failures) => {
                        job_running.set(false);
                        close_blocked.set(false);
                        ui.set_busy(false);
                        ui.set_status_text("".into());
                        match kind {
                            "install" => ui.set_installed(true),
                            "uninstall" => ui.set_installed(false),
                            _ => {}
                        }
                        if failures.is_empty() {
                            ui.set_state_text(done_state(kind).into());
                            keep_state = kind != "install";
                            push_log(&logs, format!("{kind}: finished"), false);
                        } else {
                            let summary = format!("Completed with issues: {}", failures.join(", "));
                            ui.set_state_text(warn_state(kind).into());
                            ui.set_error_text(summary.clone().into());
                            keep_state = true;
                            push_log(&logs, summary, true);
                            fit_window_later(&ui);
                        }
                        spawn_doctor(Config::load().unwrap_or_default(), tx.clone());
                    }
                    UiEvent::JobFailed(kind, message) => {
                        job_running.set(false);
                        close_blocked.set(false);
                        ui.set_busy(false);
                        ui.set_state_text(failed_state(kind).into());
                        ui.set_status_text("".into());
                        ui.set_error_text(message.clone().into());
                        keep_state = true;
                        push_log(&logs, format!("error: {message}"), true);
                        spawn_doctor(Config::load().unwrap_or_default(), tx.clone());
                        fit_window_later(&ui);
                    }
                }
            }
        },
    );

    ui.run()
}

/// The monitor's physical resolution, if known, and the window's scale factor.
#[derive(Debug, Clone, Copy)]
struct DisplayInfo {
    resolution: Option<(u32, u32)>,
    scale: f32,
}

fn display_info(ui: &MainWindow) -> DisplayInfo {
    let window = ui.window();
    let resolution = window
        .with_winit_window(|winit| {
            winit.current_monitor().map(|monitor| {
                let size = monitor.size();
                (size.width, size.height)
            })
        })
        .flatten();
    DisplayInfo {
        resolution,
        scale: window.scale_factor(),
    }
}

/// Picks the Wine DPI for the display and logs how it was chosen.
fn display_dpi(display: DisplayInfo, tx: &Sender<UiEvent>, log_path: &Path) -> u32 {
    let x = wine::detect_x_scaling(display.resolution, display.scale);
    let dpi = wine::recommended_dpi(display.resolution, display.scale, x);
    let resolution = display
        .resolution
        .map(|(width, height)| format!("{width}x{height}"))
        .unwrap_or_else(|| "unknown resolution".into());
    job_info(
        tx,
        log_path,
        format!(
            "display: {resolution} at {:.2}x scale ({}), using Wine DPI {dpi}",
            display.scale,
            x.label()
        ),
    );
    dpi
}

fn start_job(ui: &MainWindow, job_running: &Cell<bool>, state: &str) {
    job_running.set(true);
    ui.set_busy(true);
    ui.set_error_text("".into());
    ui.set_state_text(state.into());
    ui.set_status_text("Starting…".into());
    fit_window_later(ui);
}

/// Fit on the next turn, once banners that appear or disappear with the
/// properties just set have left or joined the layout.
fn fit_window_later(ui: &MainWindow) {
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = weak.upgrade() {
            fit_window(&ui);
        }
    });
}

fn done_state(kind: &str) -> &'static str {
    match kind {
        "install" => "Installed",
        "update" => "Updated",
        "repair" => "Repaired",
        "launch" => "Started. It may take a moment to appear.",
        "uninstall" => "Uninstalled",
        _ => "Done",
    }
}

fn warn_state(kind: &str) -> &'static str {
    match kind {
        "install" => "Installed with warnings",
        "update" => "Updated with warnings",
        "repair" => "Repaired with warnings",
        _ => "Finished with warnings",
    }
}

fn failed_state(kind: &str) -> &'static str {
    match kind {
        "install" => "Installation failed",
        "update" => "Update failed",
        "repair" => "Repair failed",
        "launch" => "Couldn't open FL Studio",
        "uninstall" => "Uninstall failed",
        _ => "Something went wrong",
    }
}

/// Resizes the window to its content height. With the log open it only grows,
/// so a window the user made taller keeps its size.
fn fit_window(ui: &MainWindow) {
    let window = ui.window();
    if window.is_maximized() || window.is_fullscreen() {
        return;
    }
    let current = window.size().to_logical(window.scale_factor());
    if current.width <= 0.0 {
        return;
    }
    let fit = ui.get_fit_height();
    let height = if ui.get_details_open() {
        current.height.max(fit)
    } else {
        fit
    };
    if (height - current.height).abs() < 0.5 {
        return;
    }
    let size = LogicalSize::new(current.width, height);
    if height > current.height {
        window.set_size(size);
        return;
    }
    // Shrink once Slint has applied the smaller minimum size, otherwise the
    // window manager may clamp the request to the old minimum.
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(1), move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_size(size);
        }
    });
}

fn monospace_family() -> Option<String> {
    let output = Command::new("fc-match")
        .args(["--format=%{family[0]}", "monospace"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let family = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!family.is_empty()).then_some(family)
}

fn push_log(logs: &Rc<VecModel<LogLine>>, text: String, error: bool) {
    while logs.row_count() >= 400 {
        logs.remove(0);
    }
    logs.push(LogLine {
        text: text.into(),
        error,
    });
}

fn spawn_doctor(cfg: Config, tx: Sender<UiEvent>) {
    std::thread::spawn(
        move || match panic::catch_unwind(AssertUnwindSafe(|| doctor::run(&cfg))) {
            Ok(report) => {
                let _ = tx.send(UiEvent::Doctor(Box::new(report)));
            }
            Err(payload) => {
                let _ = tx.send(UiEvent::DoctorFailed(panic_message(&payload)));
            }
        },
    );
}

/// Run a job on its own thread. A panic still ends the job with an error,
/// so the window never stays busy forever.
fn spawn_job<F>(kind: &'static str, tx: Sender<UiEvent>, job: F)
where
    F: FnOnce(Sender<UiEvent>) + Send + 'static,
{
    std::thread::spawn(move || {
        let job_tx = tx.clone();
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| job(job_tx))) {
            let message = format!("internal error: {}", panic_message(&payload));
            let _ = tx.send(UiEvent::JobFailed(kind, message));
        }
    });
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn default_elevation() -> Elevation {
    if packages::is_root() {
        Elevation::None
    } else {
        Elevation::Pkexec
    }
}

fn spawn_install(cfg: Config, display: DisplayInfo, tx: Sender<UiEvent>) {
    spawn_job("install", tx, move |tx| {
        let prefix = cfg.prefix.clone();
        let log_path = job_log(&prefix, "install");
        let result = (|| -> anyhow::Result<Vec<String>> {
            packages::ensure_not_root()?;
            validate_prefix(&prefix)?;
            fs::create_dir_all(prefix.join("logs"))?;
            let mut send = |stream: Stream, line: &str| {
                let _ = tx.send(UiEvent::Log {
                    text: line.to_string(),
                    error: matches!(stream, Stream::Stderr),
                    status: true,
                });
            };
            let cancel_prefetch = Arc::new(AtomicBool::new(false));
            let prefetch = {
                let tx = tx.clone();
                let log_path = log_path.clone();
                let cancel = cancel_prefetch.clone();
                std::thread::spawn(move || {
                    install::prefetch_cancellable(
                        Some(log_path),
                        true,
                        Some(cancel),
                        |stream, line| {
                            let _ = tx.send(UiEvent::Log {
                                text: line.to_string(),
                                error: matches!(stream, Stream::Stderr),
                                status: false,
                            });
                        },
                    );
                })
            };
            let dpi = display_dpi(display, &tx, &log_path);
            let report = install::install(
                &SetupOptions {
                    prefix: prefix.clone(),
                    installer: None,
                    dxvk: cfg.dxvk,
                    fonts: cfg.fonts,
                    audio: cfg.audio,
                    dpi: Some(dpi),
                    dry_run: false,
                    log_path: Some(log_path.clone()),
                    install_deps: true,
                    elevation: default_elevation(),
                    distro_override: cfg.distro_override.clone(),
                },
                &mut send,
            );
            cancel_prefetch.store(true, Ordering::Relaxed);
            let _ = prefetch.join();
            let report = report?;
            Ok(report.failures)
        })();
        match result {
            Ok(failures) => {
                let _ = tx.send(UiEvent::JobDone("install", failures));
            }
            Err(error) => job_failed(&tx, "install", &log_path, &error),
        }
    });
}

fn spawn_update(cfg: Config, display: DisplayInfo, tx: Sender<UiEvent>) {
    spawn_job("update", tx, move |tx| {
        let prefix = cfg.prefix.clone();
        let log_path = job_log(&prefix, "update");
        let result = (|| -> anyhow::Result<Vec<String>> {
            packages::ensure_not_root()?;
            validate_prefix(&prefix)?;
            install::ensure_installed(&prefix)?;
            fs::create_dir_all(prefix.join("logs"))?;
            let mut send = |stream: Stream, line: &str| {
                let _ = tx.send(UiEvent::Log {
                    text: line.to_string(),
                    error: matches!(stream, Stream::Stderr),
                    status: true,
                });
            };
            let cancel_prefetch = Arc::new(AtomicBool::new(false));
            let prefetch = {
                let tx = tx.clone();
                let log_path = log_path.clone();
                let cancel = cancel_prefetch.clone();
                std::thread::spawn(move || {
                    install::prefetch_cancellable(
                        Some(log_path),
                        true,
                        Some(cancel),
                        |stream, line| {
                            let _ = tx.send(UiEvent::Log {
                                text: line.to_string(),
                                error: matches!(stream, Stream::Stderr),
                                status: false,
                            });
                        },
                    );
                })
            };
            let dpi = display_dpi(display, &tx, &log_path);
            let report = install::update(
                &SetupOptions {
                    prefix: prefix.clone(),
                    installer: None,
                    dxvk: cfg.dxvk,
                    fonts: cfg.fonts,
                    audio: cfg.audio,
                    dpi: Some(dpi),
                    dry_run: false,
                    log_path: Some(log_path.clone()),
                    install_deps: false,
                    elevation: default_elevation(),
                    distro_override: cfg.distro_override.clone(),
                },
                &mut send,
            );
            cancel_prefetch.store(true, Ordering::Relaxed);
            let _ = prefetch.join();
            let report = report?;
            Ok(report.failures)
        })();
        match result {
            Ok(failures) => {
                let _ = tx.send(UiEvent::JobDone("update", failures));
            }
            Err(error) => job_failed(&tx, "update", &log_path, &error),
        }
    });
}

fn spawn_repair(cfg: Config, display: DisplayInfo, tx: Sender<UiEvent>) {
    spawn_job("repair", tx, move |tx| {
        let prefix = cfg.prefix.clone();
        let log_path = job_log(&prefix, "repair");
        let result = (|| -> anyhow::Result<Vec<String>> {
            packages::ensure_not_root()?;
            validate_prefix(&prefix)?;
            install::ensure_installed(&prefix)?;
            fs::create_dir_all(prefix.join("logs"))?;
            let mut send = |stream: Stream, line: &str| {
                let _ = tx.send(UiEvent::Log {
                    text: line.to_string(),
                    error: matches!(stream, Stream::Stderr),
                    status: true,
                });
            };
            let dpi = display_dpi(display, &tx, &log_path);
            let report = install::repair(
                &SetupOptions {
                    prefix: prefix.clone(),
                    installer: None,
                    dxvk: cfg.dxvk,
                    fonts: cfg.fonts,
                    audio: cfg.audio,
                    dpi: Some(dpi),
                    dry_run: false,
                    log_path: Some(log_path.clone()),
                    install_deps: false,
                    elevation: default_elevation(),
                    distro_override: cfg.distro_override.clone(),
                },
                &mut send,
            )?;
            Ok(report.failures)
        })();
        match result {
            Ok(failures) => {
                let _ = tx.send(UiEvent::JobDone("repair", failures));
            }
            Err(error) => job_failed(&tx, "repair", &log_path, &error),
        }
    });
}

fn spawn_launch(cfg: Config, tx: Sender<UiEvent>) {
    spawn_job("launch", tx, move |tx| {
        let prefix = cfg.prefix.clone();
        let log_path = prefix.join("logs/launch.log");
        let result = (|| -> anyhow::Result<launch::LaunchOutcome> {
            packages::ensure_not_root()?;
            validate_prefix(&prefix)?;
            install::ensure_installed(&prefix)?;
            fs::create_dir_all(prefix.join("logs"))?;
            let _ = fs::write(&log_path, b"");
            let options = launch::LaunchOptions {
                prefix,
                args: Vec::new(),
                dry_run: false,
                log_path: Some(log_path.clone()),
            };
            let mut send = |stream: Stream, line: &str| {
                let _ = tx.send(UiEvent::Log {
                    text: line.to_string(),
                    error: matches!(stream, Stream::Stderr),
                    status: true,
                });
            };
            launch::launch(&options, &mut send)
        })();
        match result {
            Ok(outcome) => {
                let _ = tx.send(UiEvent::Log {
                    text: format!("launched {}", outcome.target.display()),
                    error: false,
                    status: true,
                });
                let _ = tx.send(UiEvent::JobDone("launch", Vec::new()));
            }
            Err(error) => job_failed(&tx, "launch", &log_path, &error),
        }
    });
}

fn spawn_uninstall(cfg: Config, prefix: PathBuf, tx: Sender<UiEvent>) {
    spawn_job("uninstall", tx, move |tx| {
        let log_path = cache_log("uninstall");
        if let Err(error) = packages::ensure_not_root() {
            job_failed(&tx, "uninstall", &log_path, &error);
            return;
        }
        let options = uninstall::UninstallOptions {
            prefix: prefix.clone(),
            dry_run: false,
            log_path: Some(log_path.clone()),
        };
        let mut send = |stream: Stream, line: &str| {
            let _ = tx.send(UiEvent::Log {
                text: line.to_string(),
                error: matches!(stream, Stream::Stderr),
                status: true,
            });
        };
        match uninstall::uninstall(&options, &mut send) {
            Ok(outcome) => {
                let mut saved = cfg.clone();
                saved.prefix = default_prefix();
                if cfg.prefix == prefix
                    && let Err(error) = saved.save()
                {
                    let _ = tx.send(UiEvent::Log {
                        text: format!("warning: {error:#}"),
                        error: true,
                        status: false,
                    });
                }
                let _ = tx.send(UiEvent::Log {
                    text: format!(
                        "removed {} entries, prefix removed={}",
                        outcome.removed_entries, outcome.prefix_removed
                    ),
                    error: false,
                    status: true,
                });
                let _ = tx.send(UiEvent::JobDone("uninstall", Vec::new()));
            }
            Err(error) => job_failed(&tx, "uninstall", &log_path, &error),
        }
    });
}

fn job_log(prefix: &Path, kind: &str) -> PathBuf {
    prefix
        .join("logs")
        .join(format!("{kind}-{}.log", now_epoch()))
}

/// Uninstall deletes the prefix, so its log lives in the cache.
fn cache_log(kind: &str) -> PathBuf {
    let base = fl_studio_linux_core::http::cache_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("fl-studio-linux"));
    let _ = fs::create_dir_all(&base);
    base.join(format!("{kind}-{}.log", now_epoch()))
}

/// Appends a line the core never saw to the job log. Skipped when the log
/// directory does not exist, e.g. after an uninstall removed the prefix.
fn append_log(path: &Path, text: &str) {
    if !path.parent().is_some_and(Path::is_dir) {
        return;
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{text}");
    }
}

fn job_info(tx: &Sender<UiEvent>, log_path: &Path, text: String) {
    append_log(log_path, &text);
    let _ = tx.send(UiEvent::Log {
        text,
        error: false,
        status: true,
    });
}

fn job_failed(tx: &Sender<UiEvent>, kind: &'static str, log_path: &Path, error: &anyhow::Error) {
    let message = format!("{error:#}");
    append_log(log_path, &format!("error: {message}"));
    let _ = tx.send(UiEvent::JobFailed(kind, message));
}

fn apply_report(
    ui: &MainWindow,
    logs: &Rc<VecModel<LogLine>>,
    report: DoctorReport,
    keep_state: bool,
) {
    let package_manager = report.package_manager.as_deref();
    let distro = match package_manager {
        Some(pm) => format!("{} ({pm})", report.os.name()),
        None => format!("{} (no supported package manager)", report.os.name()),
    };
    let gpu = if report.gpus.is_empty() {
        "None detected".to_string()
    } else {
        report
            .gpus
            .iter()
            .map(gpu_label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let found = report.deps.iter().filter(|dep| dep.found).count();
    let missing: Vec<&str> = report
        .deps
        .iter()
        .filter(|dep| dep.required && !dep.found)
        .map(|dep| dep.command)
        .chain(report.missing_libraries.iter().map(String::as_str))
        .collect();
    let has_download = deps::has_download_tool(&report.deps);
    let deps = if missing.is_empty() && has_download {
        "All present".to_string()
    } else if missing.is_empty() {
        "Missing: curl or wget".to_string()
    } else {
        format!("Missing: {}", missing.join(", "))
    };
    let wine = report
        .wine_build
        .clone()
        .unwrap_or_else(|| "Not installed".into());
    let webview = if !report.prefix_exists {
        "Not installed".to_string()
    } else if report.webview2 {
        "Installed".to_string()
    } else {
        "Not installed".to_string()
    };
    ui.set_distro_text(distro.into());
    ui.set_gpu_text(gpu.into());
    ui.set_deps_text(deps.into());
    ui.set_deps_ok(missing.is_empty() && has_download);
    ui.set_wine_text(wine.clone().into());
    ui.set_webview_text(webview.clone().into());
    ui.set_prefix_text(display_path(&report.prefix).into());
    ui.set_installed(report.installed);
    if !keep_state && !ui.get_busy() {
        let state = if report.installed {
            "Installed"
        } else {
            "Not installed"
        };
        ui.set_state_text(state.into());
    }

    let gpus = report
        .gpus
        .iter()
        .map(|gpu| format!("{} [{}]", gpu.name, gpu.vendor.label()))
        .collect::<Vec<_>>();
    push_log(
        logs,
        format!(
            "system: {} ({} family, {})",
            report.os.name(),
            report.family.label(),
            package_manager.unwrap_or("no package manager")
        ),
        false,
    );
    push_log(
        logs,
        if gpus.is_empty() {
            "gpu: none detected".to_string()
        } else {
            format!("gpu: {}", gpus.join(" | "))
        },
        false,
    );
    push_log(logs, format!("wine: {wine}"), false);
    if !report.pipewire {
        push_log(
            logs,
            "pipewire: not running; WinePipe is skipped".to_string(),
            true,
        );
    }
    push_log(logs, format!("webview2: {webview}"), false);
    let mut deps_line = format!("deps: {found}/{} found", report.deps.len());
    if !missing.is_empty() {
        deps_line.push_str(&format!(", missing: {}", missing.join(", ")));
    }
    if !has_download {
        deps_line.push_str(", missing: curl or wget");
    }
    push_log(logs, deps_line, !missing.is_empty() || !has_download);
    for error in &report.errors {
        push_log(logs, format!("error: {error}"), true);
    }
}

/// Turns an lspci name such as "NVIDIA Corporation GB203 [GeForce RTX 5080]"
/// into "NVIDIA GeForce RTX 5080".
fn gpu_label(gpu: &gpu::Gpu) -> String {
    let name = gpu.name.trim();
    let (brand, vendor_names): (&str, &[&str]) = match gpu.vendor {
        Vendor::Nvidia => ("NVIDIA", &["NVIDIA Corporation"]),
        Vendor::Amd => (
            "AMD",
            &[
                "Advanced Micro Devices, Inc. [AMD/ATI]",
                "Advanced Micro Devices, Inc. [AMD]",
                "Advanced Micro Devices, Inc.",
            ],
        ),
        Vendor::Intel => ("Intel", &["Intel Corporation"]),
        Vendor::Other => return name.to_string(),
    };
    let device = vendor_names
        .iter()
        .find_map(|vendor| name.strip_prefix(vendor))
        .unwrap_or(name)
        .trim();
    let model = match device.rfind('[') {
        Some(start) if device.ends_with(']') => &device[start + 1..device.len() - 1],
        _ => device,
    };
    if model.is_empty() {
        name.to_string()
    } else {
        format!("{brand} {model}")
    }
}

fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(vendor: Vendor, name: &str) -> String {
        gpu_label(&gpu::Gpu {
            vendor,
            name: name.to_string(),
            slot: String::new(),
            pci_id: None,
            discrete: false,
        })
    }

    #[test]
    fn gpu_labels_use_marketing_names() {
        assert_eq!(
            label(
                Vendor::Nvidia,
                "NVIDIA Corporation GB203 [GeForce RTX 5080]"
            ),
            "NVIDIA GeForce RTX 5080"
        );
        assert_eq!(
            label(
                Vendor::Amd,
                "Advanced Micro Devices, Inc. [AMD/ATI] Navi 22 [Radeon RX 6700 XT]"
            ),
            "AMD Radeon RX 6700 XT"
        );
        assert_eq!(
            label(Vendor::Intel, "Intel Corporation UHD Graphics 620"),
            "Intel UHD Graphics 620"
        );
        assert_eq!(
            label(Vendor::Other, "Red Hat, Inc. Virtio 1.0 GPU"),
            "Red Hat, Inc. Virtio 1.0 GPU"
        );
    }
}
