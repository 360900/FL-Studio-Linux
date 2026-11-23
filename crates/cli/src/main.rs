use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use fl_studio_linux_core::audio::AudioDriver;
use fl_studio_linux_core::config::{Config, default_prefix, expand_home, validate_prefix};
use fl_studio_linux_core::deps;
use fl_studio_linux_core::distro::OsRelease;
use fl_studio_linux_core::doctor::{self, DoctorReport};
use fl_studio_linux_core::http;
use fl_studio_linux_core::install::{self, SetupOptions};
use fl_studio_linux_core::launch;
use fl_studio_linux_core::manifest::now_epoch;
use fl_studio_linux_core::packages::{self, Elevation, PlanRunOptions};
use fl_studio_linux_core::runner::Stream;
use fl_studio_linux_core::uninstall;
use fl_studio_linux_core::wine;

mod display;

#[derive(Parser)]
#[command(
    name = "fl-studio-linux",
    version,
    about = "One-click FL Studio installer for Linux"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show distro, GPU, dependency and prefix status
    #[command(hide = true)]
    Doctor {
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Check and install system packages needed by the installer
    #[command(hide = true)]
    Deps {
        /// Print the planned commands without changing the system
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Elevate with pkexec instead of sudo
        #[arg(long)]
        pkexec: bool,
    },
    /// Download and configure the private Wine build
    #[command(hide = true)]
    WineSetup {
        /// Print the planned commands without downloading anything
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Wine build to install (11.22 or 11.21)
        #[arg(long)]
        version: Option<String>,
        /// Prefix override; does not change the saved config
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
    /// Install FL Studio into the configured prefix
    Install {
        /// Print the planned commands without changing the system
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Prefix override; saved to the config after a successful install
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Use a local FL Studio setup executable instead of downloading it
        #[arg(long)]
        installer: Option<PathBuf>,
        /// Skip the system dependency installation step
        #[arg(long)]
        skip_deps: bool,
        /// Install DXVK for Direct3D plugins
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        dxvk: Option<bool>,
        /// Install Microsoft fonts and the musical-symbol font
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        fonts: Option<bool>,
        /// Low-latency audio driver: winepipe or none
        #[arg(long, value_parser = ["winepipe", "none"])]
        audio: Option<String>,
        /// Wine DPI to set, 96 = 100% (default: detected from the display)
        #[arg(long, value_parser = clap::value_parser!(u32).range(96..=480))]
        dpi: Option<u32>,
    },
    /// Install the latest FL Studio build over the current one
    Update {
        /// Print the planned commands without changing the system
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Prefix override; saved to the config after a successful update
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Use a local FL Studio setup executable instead of downloading it
        #[arg(long)]
        installer: Option<PathBuf>,
        /// Wine DPI to set, 96 = 100% (default: detected from the display)
        #[arg(long, value_parser = clap::value_parser!(u32).range(96..=480))]
        dpi: Option<u32>,
    },
    /// Re-apply prefix settings, fonts, DXVK and the launcher
    Repair {
        /// Print the planned commands without changing the system
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Prefix override
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Install DXVK for Direct3D plugins
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        dxvk: Option<bool>,
        /// Install Microsoft fonts and the musical-symbol font if missing
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        fonts: Option<bool>,
        /// Low-latency audio driver: winepipe or none
        #[arg(long, value_parser = ["winepipe", "none"])]
        audio: Option<String>,
        /// Wine DPI to set, 96 = 100% (default: detected from the display)
        #[arg(long, value_parser = clap::value_parser!(u32).range(96..=480))]
        dpi: Option<u32>,
    },
    /// Launch FL Studio with the correct Wine environment
    Launch {
        /// Print the environment without starting FL Studio
        #[arg(long)]
        dry_run: bool,
        /// Prefix override; does not change the saved config
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Project files or flstudio:// links to open
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Remove the prefix and desktop entries, and reset the stored prefix
    Uninstall {
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Print what would be removed without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Prefix override
        #[arg(long)]
        prefix: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    match cli.command {
        Command::Doctor { json } => {
            let report = doctor::run(&cfg);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
            }
            Ok(())
        }
        Command::Deps {
            dry_run,
            yes,
            pkexec,
        } => run_deps(&cfg, dry_run, yes, pkexec),
        Command::WineSetup {
            dry_run,
            yes,
            version,
            prefix,
        } => run_wine_setup(&cfg, dry_run, yes, version, prefix),
        Command::Install {
            dry_run,
            yes,
            prefix,
            installer,
            skip_deps,
            dxvk,
            fonts,
            audio,
            dpi,
        } => run_install(
            &cfg, dry_run, yes, prefix, installer, skip_deps, dxvk, fonts, audio, dpi,
        ),
        Command::Update {
            dry_run,
            yes,
            prefix,
            installer,
            dpi,
        } => run_update(&cfg, dry_run, yes, prefix, installer, dpi),
        Command::Repair {
            dry_run,
            yes,
            prefix,
            dxvk,
            fonts,
            audio,
            dpi,
        } => run_repair(&cfg, dry_run, yes, prefix, dxvk, fonts, audio, dpi),
        Command::Launch {
            dry_run,
            prefix,
            args,
        } => run_launch(&cfg, dry_run, prefix, args),
        Command::Uninstall {
            yes,
            dry_run,
            prefix,
        } => run_uninstall(&cfg, yes, dry_run, prefix),
    }
}

fn print_report(report: &DoctorReport) {
    println!("fl-studio-linux doctor");
    println!(
        "  distro        {} (id={}, version={}, family={}, pkg={})",
        report.os.name(),
        report.os.id,
        report.os.version_id.as_deref().unwrap_or("-"),
        report.family.label(),
        report.package_manager.as_deref().unwrap_or("not found"),
    );

    if report.gpus.is_empty() {
        println!("  gpus          none detected");
    } else {
        for gpu in &report.gpus {
            println!("  gpu           {} [{}]", gpu.name, gpu.vendor.label());
        }
    }

    if report.vulkan_icds.is_empty() {
        println!("  vulkan        no ICD manifest found (install a Vulkan driver)");
    } else {
        println!("  vulkan        {}", report.vulkan_icds.join(", "));
    }

    println!(
        "  wine build    {}",
        report.wine_build.as_deref().unwrap_or("not installed")
    );
    println!(
        "  audio         {}",
        report.audio_driver.as_deref().unwrap_or("off")
    );
    println!(
        "  prefix        {} (exists={}, installed={})",
        report.prefix.display(),
        report.prefix_exists,
        report.installed
    );
    println!("  config        {}", report.config_path.display());
    if let Some(exe) = &report.fl_exe {
        println!("  FL Studio     {}", exe.display());
    }
    println!(
        "  WebView2      {}",
        if report.webview2 { "yes" } else { "no" }
    );

    println!(
        "  pipewire      {}",
        if report.pipewire {
            "running"
        } else {
            "not running (WinePipe is skipped)"
        }
    );
    if report.missing_libraries.is_empty() {
        println!("  libraries     ok");
    } else {
        println!(
            "  libraries     MISSING {}",
            report.missing_libraries.join(", ")
        );
    }

    let found = report.deps.iter().filter(|dep| dep.found).count();
    println!("  dependencies  {}/{} found", found, report.deps.len());
    for dep in &report.deps {
        let state = if dep.found {
            "ok"
        } else if dep.required {
            "MISSING"
        } else {
            "optional"
        };
        let version = dep.version.as_deref().unwrap_or("");
        println!("    {:<11} {:<9} {}", dep.command, state, version);
    }

    if let Some(manifest) = &report.manifest {
        println!(
            "  manifest      {} steps recorded, incomplete: {}",
            manifest.steps.len(),
            manifest.incomplete_steps().len()
        );
    }

    for note in &report.package_notes {
        println!("  note          {note}");
    }

    for error in &report.errors {
        println!("  error         {error}");
    }
}

fn resolve_prefix(prefix: Option<PathBuf>, cfg: &Config) -> PathBuf {
    expand_home(&prefix.unwrap_or_else(|| cfg.prefix.clone()))
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn run_wine_setup(
    cfg: &Config,
    dry_run: bool,
    yes: bool,
    version: Option<String>,
    prefix: Option<PathBuf>,
) -> Result<()> {
    if let Some(version) = &version
        && version != wine::WINE_VERSION
        && version != wine::WINE_FALLBACK_VERSION
    {
        bail!(
            "unsupported Wine version '{version}' (expected {} or {})",
            wine::WINE_VERSION,
            wine::WINE_FALLBACK_VERSION
        );
    }

    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    println!("wine setup for {}", prefix.display());
    println!(
        "  profile {}",
        version
            .as_deref()
            .map(|version| format!("Wine {version}"))
            .unwrap_or_else(|| format!(
                "Wine {} (fallback {})",
                wine::WINE_VERSION,
                wine::WINE_FALLBACK_VERSION
            ))
    );

    if !dry_run && !yes && !confirm("Download and configure the private Wine build?")? {
        println!("aborted");
        return Ok(());
    }

    let log_path = if dry_run {
        None
    } else {
        let log_dir = prefix.join("logs");
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join(format!("wine-{}.log", now_epoch()));
        println!("  log     {}", path.display());
        Some(path)
    };

    let options = wine::SetupOptions {
        prefix,
        dry_run,
        log_path,
        wine_version: version,
        dpi: None,
    };
    let report = wine::setup(&options, print_stream)?;
    if let Some(version) = &report.wine_version {
        println!("wine    {version}");
    }
    Ok(())
}

/// An explicit `--dpi` wins; otherwise the display decides. Without a
/// detectable display the prefix's DPI is left alone.
fn resolve_dpi(dpi: Option<u32>) -> Option<u32> {
    if let Some(dpi) = dpi {
        println!("  dpi       {dpi} (--dpi)");
        return Some(dpi);
    }
    match display::detect_dpi() {
        Some((dpi, summary)) => {
            println!("  dpi       {summary}");
            Some(dpi)
        }
        None => {
            println!("  dpi       unchanged (no display detected)");
            None
        }
    }
}

fn print_stream(stream: Stream, line: &str) {
    match stream {
        Stream::Stdout => println!("{line}"),
        Stream::Stderr => eprintln!("{line}"),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "ok" } else { "skipped" }
}

fn resolve_options(
    cfg: &Config,
    dxvk: Option<bool>,
    fonts: Option<bool>,
    audio: Option<String>,
) -> (bool, bool, AudioDriver) {
    let audio = match audio.as_deref() {
        Some("none") => AudioDriver::None,
        Some("winepipe") => AudioDriver::WinePipe,
        _ => cfg.audio,
    };
    (dxvk.unwrap_or(cfg.dxvk), fonts.unwrap_or(cfg.fonts), audio)
}

#[allow(clippy::too_many_arguments)]
fn run_install(
    cfg: &Config,
    dry_run: bool,
    yes: bool,
    prefix: Option<PathBuf>,
    installer: Option<PathBuf>,
    skip_deps: bool,
    dxvk: Option<bool>,
    fonts: Option<bool>,
    audio: Option<String>,
    dpi: Option<u32>,
) -> Result<()> {
    let os = OsRelease::detect_with_override(cfg.distro_override.as_deref())?;
    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    let installer = installer.map(|path| expand_home(&path));
    let (dxvk, fonts, audio) = resolve_options(cfg, dxvk, fonts, audio);

    println!("install FL Studio");
    println!("  distro    {} ({})", os.name(), os.family().label());
    println!("  prefix    {}", prefix.display());
    println!(
        "  installer {}",
        installer
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "download from image-line.com".to_string())
    );
    println!(
        "  deps      {}",
        if skip_deps {
            "skipped"
        } else {
            "install if missing"
        }
    );
    println!(
        "  wine      bootstrap {} (fallback {})",
        wine::WINE_VERSION,
        wine::WINE_FALLBACK_VERSION
    );
    println!("  fonts     {}", if fonts { "on" } else { "off" });
    println!("  dxvk      {}", if dxvk { "on" } else { "off" });
    println!("  audio     {}", audio.title());
    let dpi = resolve_dpi(dpi);
    println!();

    let options = SetupOptions {
        prefix: prefix.clone(),
        installer,
        dxvk,
        fonts,
        audio,
        dpi,
        dry_run,
        log_path: None,
        install_deps: !skip_deps,
        elevation: packages::default_elevation(),
        distro_override: cfg.distro_override.clone(),
    };

    if dry_run {
        install::install(&options, print_stream)?;
        return Ok(());
    }

    if !yes && !confirm(&format!("Install FL Studio into {}?", prefix.display()))? {
        println!("aborted");
        return Ok(());
    }

    fs::create_dir_all(&prefix)?;
    let log_dir = prefix.join("logs");
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("install-{}.log", now_epoch()));
    println!("  log       {}", log_path.display());

    let cancel_prefetch = Arc::new(AtomicBool::new(false));
    let prefetch = {
        let log_path = log_path.clone();
        let with_installer = options.installer.is_none();
        let cancel = cancel_prefetch.clone();
        std::thread::spawn(move || {
            install::prefetch_cancellable(
                Some(log_path),
                with_installer,
                Some(cancel),
                print_stream,
            )
        })
    };

    let mut options = options;
    options.log_path = Some(log_path);
    let outcome = install::install(&options, print_stream);
    cancel_prefetch.store(true, Ordering::Relaxed);
    let _ = prefetch.join();
    let outcome = outcome?;
    print_outcome(&outcome);

    let mut saved = cfg.clone();
    saved.prefix = prefix;
    saved.dxvk = dxvk;
    saved.fonts = fonts;
    saved.audio = audio;
    saved.save()?;
    println!("saved install location and options in the config");
    Ok(())
}

fn print_outcome(outcome: &install::SetupReport) {
    println!(
        "  wine          {}",
        outcome.wine_version.as_deref().unwrap_or("-")
    );
    if let Some(exe) = &outcome.fl_exe {
        println!("  FL Studio     {}", exe.display());
    }
    println!("  fonts         {}", yes_no(outcome.fonts));
    println!("  plugin modes  {}", yes_no(outcome.runtimes));
    println!(
        "  dxvk          {}",
        outcome.dxvk_version.as_deref().unwrap_or("skipped")
    );
    println!(
        "  audio         {}",
        outcome.audio_driver.as_deref().unwrap_or("off")
    );
    println!("  webview2      {}", yes_no(outcome.webview2));
    println!("  desktop entry {}", yes_no(outcome.desktop));
    println!("  url handler   {}", yes_no(outcome.url_handler));

    if !outcome.failures.is_empty() {
        println!("completed with {} warning(s):", outcome.failures.len());
        for failure in &outcome.failures {
            println!("  - {failure}");
        }
    }
    println!("Sign in with your Image-Line account inside FL Studio to unlock it.");
}

fn run_update(
    cfg: &Config,
    dry_run: bool,
    yes: bool,
    prefix: Option<PathBuf>,
    installer: Option<PathBuf>,
    dpi: Option<u32>,
) -> Result<()> {
    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    install::ensure_installed(&prefix)?;
    let installer = installer.map(|path| expand_home(&path));
    println!("update FL Studio");
    println!("  prefix    {}", prefix.display());
    let dpi = resolve_dpi(dpi);

    if !dry_run && !yes && !confirm(&format!("Update FL Studio in {}?", prefix.display()))? {
        println!("aborted");
        return Ok(());
    }

    let log_path = if dry_run {
        None
    } else {
        let log_dir = prefix.join("logs");
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join(format!("update-{}.log", now_epoch()));
        println!("  log       {}", path.display());
        Some(path)
    };

    let options = SetupOptions {
        prefix: prefix.clone(),
        installer,
        dxvk: cfg.dxvk,
        fonts: cfg.fonts,
        audio: cfg.audio,
        dpi,
        dry_run,
        log_path,
        install_deps: false,
        elevation: packages::default_elevation(),
        distro_override: cfg.distro_override.clone(),
    };
    let outcome = install::update(&options, print_stream)?;
    print_outcome(&outcome);

    if !dry_run {
        let mut saved = cfg.clone();
        saved.prefix = prefix;
        saved.save()?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_repair(
    cfg: &Config,
    dry_run: bool,
    yes: bool,
    prefix: Option<PathBuf>,
    dxvk: Option<bool>,
    fonts: Option<bool>,
    audio: Option<String>,
    dpi: Option<u32>,
) -> Result<()> {
    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    install::ensure_installed(&prefix)?;
    let (dxvk, fonts, audio) = resolve_options(cfg, dxvk, fonts, audio);
    println!("repair FL Studio");
    println!("  prefix    {}", prefix.display());
    let dpi = resolve_dpi(dpi);

    if !dry_run && !yes && !confirm(&format!("Repair FL Studio in {}?", prefix.display()))? {
        println!("aborted");
        return Ok(());
    }

    let log_path = if dry_run {
        None
    } else {
        let log_dir = prefix.join("logs");
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join(format!("repair-{}.log", now_epoch()));
        println!("  log       {}", path.display());
        Some(path)
    };

    let options = SetupOptions {
        prefix: prefix.clone(),
        installer: None,
        dxvk,
        fonts,
        audio,
        dpi,
        dry_run,
        log_path,
        install_deps: false,
        elevation: packages::default_elevation(),
        distro_override: cfg.distro_override.clone(),
    };
    let outcome = install::repair(&options, print_stream)?;
    print_outcome(&outcome);

    let mut saved = cfg.clone();
    saved.prefix = prefix;
    saved.dxvk = dxvk;
    saved.fonts = fonts;
    saved.audio = audio;
    if !dry_run {
        saved.save()?;
    }
    Ok(())
}

fn run_launch(
    cfg: &Config,
    dry_run: bool,
    prefix: Option<PathBuf>,
    args: Vec<String>,
) -> Result<()> {
    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    install::ensure_installed(&prefix)?;
    let log_path = if dry_run {
        None
    } else {
        let log_dir = prefix.join("logs");
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join("launch.log");
        // A link goes to the running FL Studio; keep that launch's log.
        if !args.iter().any(|arg| launch::is_url(arg)) {
            let _ = fs::write(&path, b"");
        }
        Some(path)
    };
    let options = launch::LaunchOptions {
        prefix,
        args,
        dry_run,
        log_path,
    };
    let outcome = launch::launch(&options, print_stream)?;
    println!("target  {}", outcome.target.display());
    Ok(())
}

fn run_uninstall(cfg: &Config, yes: bool, dry_run: bool, prefix: Option<PathBuf>) -> Result<()> {
    let prefix = resolve_prefix(prefix, cfg);
    validate_prefix(&prefix)?;
    if !dry_run
        && !yes
        && !confirm(&format!(
            "Remove {} and its desktop entries?",
            prefix.display()
        ))?
    {
        println!("aborted");
        return Ok(());
    }
    let log_path = if dry_run {
        None
    } else {
        let log_dir = http::cache_dir()?.join("logs");
        fs::create_dir_all(&log_dir)?;
        Some(log_dir.join(format!("uninstall-{}.log", now_epoch())))
    };
    let options = uninstall::UninstallOptions {
        prefix: prefix.clone(),
        dry_run,
        log_path,
    };
    let outcome = uninstall::uninstall(&options, print_stream)?;
    if dry_run {
        return Ok(());
    }
    if prefix == cfg.prefix {
        let mut saved = cfg.clone();
        saved.prefix = default_prefix();
        saved.save()?;
        println!("install location reset to {}", saved.prefix.display());
    }
    println!(
        "removed {} desktop entr{}, prefix removed={}",
        outcome.removed_entries,
        if outcome.removed_entries == 1 {
            "y"
        } else {
            "ies"
        },
        outcome.prefix_removed
    );
    Ok(())
}

fn format_missing_deps(missing: &[&deps::DepCheck], has_download: bool) -> String {
    let names: Vec<&str> = missing.iter().map(|dep| dep.command).collect();
    if names.is_empty() && !has_download {
        "curl or wget".to_string()
    } else if !has_download {
        format!("{} and curl or wget", names.join(", "))
    } else {
        names.join(", ")
    }
}

fn run_deps(cfg: &Config, dry_run: bool, yes: bool, pkexec: bool) -> Result<()> {
    let os = OsRelease::detect_with_override(cfg.distro_override.as_deref())?;
    let plan = packages::plan(&os);

    println!(
        "dependency plan for {} ({})",
        os.name(),
        plan.family.label()
    );
    println!("  flavor  {}", plan.flavor);
    println!("  steps   {}", plan.steps.len());
    for note in &plan.notes {
        println!("  note    {note}");
    }

    if plan.steps.is_empty() {
        let checks = deps::check_all();
        let missing = deps::missing(&checks);
        let has_download = deps::has_download_tool(&checks);
        if missing.is_empty() && has_download {
            println!("all required dependencies are present");
            return Ok(());
        }
        bail!(
            "no automatic plan for this distribution; still missing: {}",
            format_missing_deps(&missing, has_download)
        );
    }

    let print_line = |stream: Stream, line: &str| match stream {
        Stream::Stdout => println!("{line}"),
        Stream::Stderr => eprintln!("{line}"),
    };

    if dry_run {
        let options = PlanRunOptions {
            dry_run: true,
            elevation: packages::default_elevation(),
            log_path: None,
        };
        packages::run_plan(&plan, &options, print_line)?;
        return Ok(());
    }

    if !yes && !confirm("Proceed?")? {
        println!("aborted");
        return Ok(());
    }

    let elevation = if pkexec {
        Elevation::Pkexec
    } else if packages::is_root() {
        Elevation::None
    } else {
        Elevation::Sudo
    };
    match elevation {
        Elevation::Pkexec if deps::which("pkexec").is_none() => {
            bail!("pkexec was not found on PATH")
        }
        Elevation::Sudo if deps::which("sudo").is_none() => {
            bail!("sudo was not found; run as root or install sudo")
        }
        _ => {}
    }

    validate_prefix(&cfg.prefix)?;
    let log_dir = cfg.prefix.join("logs");
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("deps-{}.log", now_epoch()));
    println!("log     {}", log_path.display());

    let options = PlanRunOptions {
        dry_run: false,
        elevation,
        log_path: Some(log_path),
    };
    let outcome = packages::run_plan(&plan, &options, print_line)?;
    println!("completed {} steps", outcome.completed.len());
    if !outcome.failed_optional.is_empty() {
        println!(
            "optional steps failed: {}",
            outcome.failed_optional.join(", ")
        );
    }

    let checks = deps::check_all();
    let missing = deps::missing(&checks);
    let has_download = deps::has_download_tool(&checks);
    if missing.is_empty() && has_download {
        println!("all required dependencies are present");
        Ok(())
    } else {
        bail!(
            "still missing: {}",
            format_missing_deps(&missing, has_download)
        )
    }
}
