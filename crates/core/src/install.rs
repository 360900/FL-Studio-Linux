//! End-to-end FL Studio install, update and repair.
//!
//! The pipeline is: host dependencies, private Wine, fonts and plugin
//! runtimes, optional DXVK, Image-Line's setup, WebView2 keys and the desktop
//! launcher. Each stage records its result in the prefix manifest.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::audio::AudioDriver;
use crate::distro::OsRelease;
use crate::journal::Report;
use crate::manifest::{
    Manifest, STEP_AUDIO, STEP_COMPONENTS, STEP_DEPS, STEP_DESKTOP, STEP_DXVK, STEP_FONTS,
    STEP_INSTALLER, STEP_PREFIX, STEP_WEBVIEW2, STEP_WINE, StepStatus,
};
use crate::packages::{Elevation, PlanRunOptions};
use crate::runner::Stream;
use crate::{app_exe, audio, components, deps, desktop, dxvk, flstudio, installer, packages, wine};

/// Steps run inside this module stream through its report, which already
/// writes the log; giving them the path too doubled every line.
const CHILD_LOG: Option<PathBuf> = None;

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub prefix: PathBuf,
    pub installer: Option<PathBuf>,
    pub dxvk: bool,
    pub fonts: bool,
    /// Low-latency ASIO driver to install.
    pub audio: AudioDriver,
    pub dpi: Option<u32>,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
    /// Install missing host packages first.
    pub install_deps: bool,
    pub elevation: Elevation,
}

#[derive(Debug, Clone, Default)]
pub struct SetupReport {
    pub wine_version: Option<String>,
    pub fl_exe: Option<PathBuf>,
    pub fonts: bool,
    pub runtimes: bool,
    pub audio_driver: Option<String>,
    pub dxvk_version: Option<String>,
    pub webview2: bool,
    pub desktop: bool,
    pub url_handler: bool,
    pub failures: Vec<String>,
}

pub fn install<F>(opts: &SetupOptions, on_line: F) -> Result<SetupReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let mut outcome = SetupReport::default();

    crate::config::validate_prefix(&opts.prefix)?;
    report.info("FL Studio installation");

    if opts.install_deps {
        if !install_dependencies(opts, &mut report)? {
            outcome.failures.push("host dependencies".to_string());
        }
    } else {
        report.info("host dependencies: skipped");
        mark(
            &opts.prefix,
            STEP_DEPS,
            StepStatus::Skipped,
            None,
            opts.dry_run,
        );
    }

    let setup = wine::setup(
        &wine::SetupOptions {
            prefix: opts.prefix.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
            wine_version: None,
            dpi: opts.dpi,
        },
        |stream, line| report.line(stream, line),
    )?;
    outcome.wine_version = setup.wine_version;
    mark(
        &opts.prefix,
        STEP_PREFIX,
        StepStatus::Done,
        None,
        opts.dry_run,
    );
    mark(
        &opts.prefix,
        STEP_WINE,
        StepStatus::Done,
        outcome.wine_version.clone(),
        opts.dry_run,
    );

    let components = components::install(
        &opts.prefix,
        opts.fonts,
        opts.dry_run,
        CHILD_LOG,
        |stream, line| report.line(stream, line),
    )?;
    outcome.fonts = components.fonts;
    outcome.runtimes = components.runtimes;
    outcome.failures.extend(components.failures.iter().cloned());
    mark(
        &opts.prefix,
        STEP_COMPONENTS,
        status(components.runtimes),
        None,
        opts.dry_run,
    );
    mark(
        &opts.prefix,
        STEP_FONTS,
        if opts.fonts {
            status(components.fonts)
        } else {
            StepStatus::Skipped
        },
        None,
        opts.dry_run,
    );

    install_audio(opts, &mut report, &mut outcome);

    apply_dxvk(opts, &mut report, &mut outcome)?;
    // winetricks can leave its own Windows version behind.
    wine::reapply_windows(
        &opts.prefix,
        opts.dpi,
        opts.dry_run,
        CHILD_LOG,
        |stream, line| report.line(stream, line),
    )?;

    let installed = installer::install(
        &installer::InstallerOptions {
            prefix: opts.prefix.clone(),
            installer: opts.installer.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
        },
        |stream, line| report.line(stream, line),
    )?;
    outcome.fl_exe = Some(installed.fl_exe.clone());
    mark(
        &opts.prefix,
        STEP_INSTALLER,
        StepStatus::Done,
        None,
        opts.dry_run,
    );

    finish_install(opts, &mut report, &mut outcome, true)?;
    Ok(outcome)
}

/// Run the latest installer over the current installation.
pub fn update<F>(opts: &SetupOptions, on_line: F) -> Result<SetupReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let mut outcome = SetupReport::default();

    crate::config::validate_prefix(&opts.prefix)?;
    report.info("FL Studio update");
    ensure_installed(&opts.prefix)?;

    let setup = wine::setup(
        &wine::SetupOptions {
            prefix: opts.prefix.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
            wine_version: None,
            dpi: opts.dpi,
        },
        |stream, line| report.line(stream, line),
    )?;
    outcome.wine_version = setup.wine_version;

    install_audio(opts, &mut report, &mut outcome);
    let installed = installer::install(
        &installer::InstallerOptions {
            prefix: opts.prefix.clone(),
            installer: opts.installer.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
        },
        |stream, line| report.line(stream, line),
    )?;
    outcome.fl_exe = Some(installed.fl_exe.clone());
    mark(
        &opts.prefix,
        STEP_INSTALLER,
        StepStatus::Done,
        None,
        opts.dry_run,
    );

    finish_install(opts, &mut report, &mut outcome, true)?;
    Ok(outcome)
}

/// Re-apply prefix settings, missing fonts, DXVK and the launcher.
pub fn repair<F>(opts: &SetupOptions, on_line: F) -> Result<SetupReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let mut outcome = SetupReport::default();

    crate::config::validate_prefix(&opts.prefix)?;
    report.info("FL Studio repair");
    ensure_installed(&opts.prefix)?;

    let setup = wine::setup(
        &wine::SetupOptions {
            prefix: opts.prefix.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
            wine_version: None,
            dpi: opts.dpi,
        },
        |stream, line| report.line(stream, line),
    )?;
    outcome.wine_version = setup.wine_version;

    if opts.fonts {
        let fonts =
            components::ensure_fonts(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
                report.line(stream, line)
            })?;
        outcome.fonts = fonts;
        mark(&opts.prefix, STEP_FONTS, status(fonts), None, opts.dry_run);
    } else {
        mark(
            &opts.prefix,
            STEP_FONTS,
            StepStatus::Skipped,
            None,
            opts.dry_run,
        );
    }

    let runtimes =
        components::ensure_runtimes(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
            report.line(stream, line)
        })?;
    outcome.runtimes = runtimes;
    mark(
        &opts.prefix,
        STEP_COMPONENTS,
        status(runtimes),
        None,
        opts.dry_run,
    );
    if !runtimes {
        outcome.failures.push("plugin runtimes".to_string());
    }
    install_audio(opts, &mut report, &mut outcome);
    wine::reapply_windows(
        &opts.prefix,
        opts.dpi,
        opts.dry_run,
        CHILD_LOG,
        |stream, line| report.line(stream, line),
    )?;

    apply_dxvk(opts, &mut report, &mut outcome)?;
    finish_install(opts, &mut report, &mut outcome, false)?;
    Ok(outcome)
}

fn finish_install(
    opts: &SetupOptions,
    report: &mut Report<'_>,
    outcome: &mut SetupReport,
    apply_webview: bool,
) -> Result<()> {
    // The runtime usually comes from Image-Line's installer, but it is
    // optional there and sometimes skipped. Install it when it is missing.
    if !crate::components::webview2_installed(&opts.prefix) {
        let _ =
            components::install_webview2(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
                report.line(stream, line)
            });
    }
    outcome.webview2 = crate::components::webview2_installed(&opts.prefix);
    mark(
        &opts.prefix,
        STEP_WEBVIEW2,
        status(outcome.webview2),
        None,
        opts.dry_run,
    );
    if !opts.dry_run && !outcome.webview2 {
        report.warn("WebView2 was not found; FL Cloud, Help and the Sounds tab may be blank");
        outcome
            .failures
            .push("WebView2 runtime not found".to_string());
    }
    if apply_webview {
        wine::configure_webview2_prefix(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
            report.line(stream, line)
        })?;
    }
    if let Err(error) = flstudio::configure(
        &opts.prefix,
        opts.audio,
        opts.dry_run,
        CHILD_LOG,
        |stream, line| report.line(stream, line),
    ) {
        report.warn(format!("FL Studio settings failed: {error}"));
        outcome
            .failures
            .push(format!("FL Studio settings: {error}"));
    }

    let desktop = desktop::create(
        &desktop::DesktopOptions {
            prefix: opts.prefix.clone(),
            dry_run: opts.dry_run,
            log_path: CHILD_LOG,
        },
        |stream, line| report.line(stream, line),
    );
    match desktop {
        Ok(result) => {
            outcome.desktop = true;
            outcome.url_handler = result.url_handler;
            mark(
                &opts.prefix,
                STEP_DESKTOP,
                StepStatus::Done,
                None,
                opts.dry_run,
            );
        }
        Err(error) => {
            report.warn(format!("desktop entry failed: {error}"));
            mark(
                &opts.prefix,
                STEP_DESKTOP,
                StepStatus::Failed,
                Some(error.to_string()),
                opts.dry_run,
            );
            outcome.failures.push(format!("desktop: {error}"));
        }
    }

    if !opts.dry_run {
        if let Ok(mut manifest) = Manifest::load_or_new(&opts.prefix) {
            manifest.fl_exe = app_exe(&opts.prefix).map(|path| path.display().to_string());
            manifest.wine_version = outcome.wine_version.clone();
            let _ = manifest.save(&opts.prefix);
        }
        report.info(format!(
            "finished with {} failure(s)",
            outcome.failures.len()
        ));
    }
    Ok(())
}

fn install_audio(opts: &SetupOptions, report: &mut Report<'_>, outcome: &mut SetupReport) {
    match audio::install(
        &opts.prefix,
        opts.audio,
        opts.dry_run,
        CHILD_LOG,
        |stream, line| report.line(stream, line),
    ) {
        Ok(result) => {
            outcome.audio_driver = result.installed.then(|| result.driver.title().to_string());
            let status = if result.installed {
                StepStatus::Done
            } else {
                StepStatus::Skipped
            };
            mark(&opts.prefix, STEP_AUDIO, status, None, opts.dry_run);
        }
        Err(error) => {
            report.warn(format!("low-latency audio failed: {error}"));
            mark(
                &opts.prefix,
                STEP_AUDIO,
                StepStatus::Failed,
                Some(error.to_string()),
                opts.dry_run,
            );
            outcome.failures.push(format!("audio: {error}"));
        }
    }
}

fn apply_dxvk(
    opts: &SetupOptions,
    report: &mut Report<'_>,
    outcome: &mut SetupReport,
) -> Result<()> {
    if opts.dxvk {
        match dxvk::install(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
            report.line(stream, line)
        }) {
            Ok(version) => {
                outcome.dxvk_version = version;
                mark(
                    &opts.prefix,
                    STEP_DXVK,
                    StepStatus::Done,
                    None,
                    opts.dry_run,
                );
            }
            Err(error) => {
                report.warn(format!("DXVK setup failed: {error}"));
                mark(
                    &opts.prefix,
                    STEP_DXVK,
                    StepStatus::Failed,
                    Some(error.to_string()),
                    opts.dry_run,
                );
                outcome.failures.push(format!("dxvk: {error}"));
            }
        }
    } else {
        if let Err(error) = dxvk::remove(&opts.prefix, opts.dry_run, CHILD_LOG, |stream, line| {
            report.line(stream, line)
        }) {
            report.warn(format!("removing DXVK failed: {error}"));
        }
        mark(
            &opts.prefix,
            STEP_DXVK,
            StepStatus::Skipped,
            None,
            opts.dry_run,
        );
    }
    Ok(())
}

fn install_dependencies(opts: &SetupOptions, report: &mut Report<'_>) -> Result<bool> {
    let checks = deps::check_all();
    let missing = deps::missing(&checks);
    let has_download = deps::has_download_tool(&checks);
    if missing.is_empty() && has_download {
        report.info("host dependencies are already present");
        mark(
            &opts.prefix,
            STEP_DEPS,
            StepStatus::Done,
            None,
            opts.dry_run,
        );
        return Ok(true);
    }

    let os = match OsRelease::detect() {
        Ok(os) => os,
        Err(error) => {
            report.warn(format!("could not detect the distribution: {error:#}"));
            return fail_deps(opts, report, &missing, has_download);
        }
    };
    let plan = packages::plan(&os);
    for note in &plan.notes {
        report.info(format!("note: {note}"));
    }
    if plan.steps.is_empty() {
        return fail_deps(opts, report, &missing, has_download);
    }

    let elevation = opts.elevation;
    if elevation == Elevation::Sudo && deps::which("sudo").is_none() {
        bail!("sudo is missing; install the dependencies manually or run as root");
    }
    if elevation == Elevation::Pkexec && deps::which("pkexec").is_none() {
        bail!("pkexec is missing; install the dependencies manually or run as root");
    }

    let options = PlanRunOptions {
        dry_run: opts.dry_run,
        elevation,
        // The report above already tees every streamed line into the log.
        log_path: None,
    };
    let outcome = packages::run_plan(&plan, &options, |stream, line| report.line(stream, line))?;
    if !outcome.failed_optional.is_empty() {
        report.warn(format!(
            "optional dependency steps failed: {}",
            outcome.failed_optional.join(", ")
        ));
    }
    mark(
        &opts.prefix,
        STEP_DEPS,
        StepStatus::Done,
        None,
        opts.dry_run,
    );
    Ok(true)
}

fn fail_deps(
    opts: &SetupOptions,
    report: &mut Report<'_>,
    missing: &[&crate::deps::DepCheck],
    has_download: bool,
) -> Result<bool> {
    let names: Vec<&str> = missing.iter().map(|check| check.command).collect();
    let detail = if names.is_empty() && !has_download {
        "no curl or wget".to_string()
    } else if !has_download {
        format!("{} and curl or wget", names.join(", "))
    } else {
        names.join(", ")
    };
    report.warn(format!(
        "no automatic dependency plan for this distribution; still missing: {detail}"
    ));
    mark(
        &opts.prefix,
        STEP_DEPS,
        StepStatus::Failed,
        Some(detail.clone()),
        opts.dry_run,
    );
    bail!("missing required dependencies: {detail}. Install them, then run the installer again.");
}

/// Downloads the FL Studio setup into the shared cache. Meant for a background
/// thread started with the install, so the download overlaps Wine setup;
/// `installer::install` then finds it cached.
pub fn prefetch<F>(log_path: Option<PathBuf>, with_installer: bool, on_line: F)
where
    F: FnMut(Stream, &str),
{
    if !with_installer {
        return;
    }
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    report.info("downloading the FL Studio installer in the background");
    if let Err(error) = installer::fetch_installer(&mut report) {
        report.warn(format!("background download failed: {error:#}"));
    }
}

pub fn ensure_installed(prefix: &Path) -> Result<PathBuf> {
    let Some(exe) = app_exe(prefix) else {
        bail!(
            "FL Studio is not installed in {}; run the installer again",
            prefix.display()
        );
    };
    Ok(exe)
}

fn status(ok: bool) -> StepStatus {
    if ok {
        StepStatus::Done
    } else {
        StepStatus::Failed
    }
}

fn mark(prefix: &Path, step: &str, status: StepStatus, detail: Option<String>, dry_run: bool) {
    if dry_run {
        return;
    }
    if let Ok(mut manifest) = Manifest::load_or_new(prefix) {
        manifest.mark(step, status, detail);
        let _ = manifest.save(prefix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(prefix: &Path) -> SetupOptions {
        SetupOptions {
            prefix: prefix.to_path_buf(),
            installer: None,
            dxvk: true,
            fonts: true,
            audio: AudioDriver::WinePipe,
            dpi: None,
            dry_run: true,
            log_path: None,
            install_deps: false,
            elevation: Elevation::None,
        }
    }

    #[test]
    fn dry_run_covers_the_whole_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        let mut lines = Vec::new();
        let report = install(&options(&prefix), |_, line| lines.push(line.to_string())).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("wine-11.22-amd64.tar.xz"));
        assert!(text.contains("FL Studio setup"));
        assert!(text.contains("corefonts"));
        assert!(text.contains("FL64.exe"));
        assert!(text.contains("fl-studio.desktop"));
        assert!(report.fl_exe.is_some());
        assert!(!prefix.exists());
    }

    #[test]
    fn repair_needs_an_installed_copy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repair(&options(dir.path()), |_, _| {}).is_err());
    }

    #[test]
    fn status_maps_booleans() {
        assert_eq!(status(true), StepStatus::Done);
        assert_eq!(status(false), StepStatus::Failed);
    }
}
