//! Per-distribution dependency installation plans.
//!
//! The FL Studio installer uses its own Wine build, but the host needs the
//! regular Wine libraries (including their 32-bit variants), winetricks and
//! the archive tools winetricks downloads fonts with. Commands are rendered as
//! shell snippets so they can be shown in a dry run, logged, and executed with
//! sudo or pkexec.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::distro::{Family, OsRelease};
use crate::runner::{self, RunOptions, Stream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStep {
    pub label: String,
    pub script: String,
    pub needs_root: bool,
    pub optional: bool,
}

impl PackageStep {
    fn root(label: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            script: script.into(),
            needs_root: true,
            optional: false,
        }
    }

    fn root_optional(label: impl Into<String>, script: impl Into<String>) -> Self {
        Self {
            optional: true,
            ..Self::root(label, script)
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackagePlan {
    pub family: Family,
    pub flavor: String,
    pub steps: Vec<PackageStep>,
    pub notes: Vec<String>,
}

pub fn plan(os: &OsRelease) -> PackagePlan {
    plan_with(os, None, None)
}

fn arch_multilib_enabled() -> bool {
    fs::read_to_string("/etc/pacman.conf")
        .map(|text| text.lines().any(|line| line.trim() == "[multilib]"))
        .unwrap_or(true)
}

fn is_ostree_host() -> bool {
    Path::new("/run/ostree-booted").exists()
}

fn plan_with(os: &OsRelease, multilib: Option<bool>, ostree: Option<bool>) -> PackagePlan {
    let family = os.family();
    let id = os.id.to_ascii_lowercase();
    let mut notes = Vec::new();

    if id == "manjaro" {
        notes.push(
            "Manjaro is not officially supported. The Arch plan may still work, but expect rough edges."
                .to_string(),
        );
    }
    if id.contains("bazzite") || ostree.unwrap_or_else(is_ostree_host) {
        notes.push(format!(
            "{} is an immutable distribution; automatic dependency installation is disabled.",
            os.name()
        ));
        return PackagePlan {
            family,
            flavor: id,
            steps: Vec::new(),
            notes,
        };
    }

    let (flavor, steps) = match family {
        Family::Arch => arch_plan(multilib.unwrap_or_else(arch_multilib_enabled)),
        Family::Ubuntu => ubuntu_plan(os),
        Family::Fedora => fedora_plan(),
        Family::Suse => suse_plan(),
        Family::Debian => {
            notes.push(
                "Debian is not officially supported. Install wine, winetricks, cabextract, wget, curl, 7z, tar, xz and unzip manually."
                    .to_string(),
            );
            ("debian".to_string(), Vec::new())
        }
        Family::Unknown => {
            notes.push(
                "Unknown distribution. Install wine, winetricks, cabextract, wget, curl, 7z, tar, xz and unzip manually."
                    .to_string(),
            );
            ("unknown".to_string(), Vec::new())
        }
    };

    PackagePlan {
        family,
        flavor,
        steps,
        notes,
    }
}

fn arch_plan(multilib_enabled: bool) -> (String, Vec<PackageStep>) {
    let mut steps = Vec::new();
    if !multilib_enabled {
        steps.push(PackageStep::root(
            "Enable the Arch multilib repository",
            "grep -q '^\\[multilib\\]' /etc/pacman.conf || printf '\\n[multilib]\\nInclude = /etc/pacman.d/mirrorlist\\n' >> /etc/pacman.conf",
        ));
    }
    steps.push(PackageStep::root(
        "Install Wine libraries and installer tools",
        pacman_install(
            "-Syu",
            "wine winetricks cabextract wget curl 7zip unzip tar xz pciutils icoutils",
        ),
    ));
    ("arch".to_string(), steps)
}

/// Skips names already satisfied by an installed package (e.g. `wine` via
/// `wine-staging`), so pacman does not try to replace a conflicting provider.
fn pacman_install(op: &str, packages: &str) -> String {
    format!(
        "missing=$(pacman -T {packages}); [ -z \"$missing\" ] || pacman {op} --needed --noconfirm $missing"
    )
}

fn ubuntu_plan(os: &OsRelease) -> (String, Vec<PackageStep>) {
    let (major, minor) = version_tuple(os);
    let (flavor, install_script) = if os.id == "ubuntu" {
        let archive = if (major, minor) >= (26, 4) {
            "7zip"
        } else {
            "p7zip-full"
        };
        (
            format!("ubuntu-{major}.{minor}"),
            format!(
                "DEBIAN_FRONTEND=noninteractive apt-get install -y wine wine64 wine32:i386 winetricks cabextract wget curl {archive} tar xz-utils pciutils unzip icoutils"
            ),
        )
    } else {
        // Ubuntu derivatives carry their own VERSION_ID scheme, so detect
        // whether the repo provides `7zip` or `p7zip-full` at install time.
        (
            os.id.clone(),
            "archive=p7zip-full; apt-cache show 7zip >/dev/null 2>&1 && archive=7zip; DEBIAN_FRONTEND=noninteractive apt-get install -y wine wine64 wine32:i386 winetricks cabextract wget curl \"$archive\" tar xz-utils pciutils unzip icoutils".to_string(),
        )
    };
    let steps = vec![
        PackageStep::root(
            "Enable the i386 architecture",
            "dpkg --add-architecture i386",
        ),
        PackageStep::root("Refresh package lists", "apt-get update"),
        PackageStep::root("Install Wine libraries and installer tools", install_script),
    ];
    (flavor, steps)
}

fn fedora_plan() -> (String, Vec<PackageStep>) {
    (
        "fedora".to_string(),
        vec![
            PackageStep::root(
                "Install Wine libraries and installer tools",
                "dnf install -y wine winetricks cabextract wget curl p7zip p7zip-plugins tar xz pciutils unzip icoutils",
            ),
            PackageStep::root(
                "Install the 32-bit Wine libraries",
                "dnf install -y wine-core.i686",
            ),
        ],
    )
}

fn suse_plan() -> (String, Vec<PackageStep>) {
    (
        "opensuse".to_string(),
        vec![
            PackageStep::root("Refresh package lists", "zypper --non-interactive refresh"),
            PackageStep::root(
                "Install Wine libraries and installer tools",
                "zypper --non-interactive install -y wine winetricks cabextract wget curl 7zip tar xz pciutils unzip icoutils",
            ),
            PackageStep::root_optional(
                "Install the 32-bit Wine libraries",
                "zypper --non-interactive install -y wine-32bit",
            ),
        ],
    )
}

fn version_tuple(os: &OsRelease) -> (u32, u32) {
    let Some(version) = os.version_id.as_deref() else {
        return (0, 0);
    };
    let mut parts = version.split('.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    None,
    Sudo,
    Pkexec,
}

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|out| out.trim() == "0")
        .unwrap_or(false)
}

pub fn default_elevation() -> Elevation {
    if is_root() {
        Elevation::None
    } else {
        Elevation::Sudo
    }
}

#[derive(Debug, Clone)]
pub struct PlanRunOptions {
    pub dry_run: bool,
    pub elevation: Elevation,
    pub log_path: Option<PathBuf>,
}

impl Default for PlanRunOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            elevation: default_elevation(),
            log_path: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct PlanOutcome {
    pub completed: Vec<String>,
    pub failed_optional: Vec<String>,
}

const OPT_FAIL_MARKER: &str = "__FL_STUDIO_OPT_FAIL__:";

pub fn run_plan<F>(plan: &PackagePlan, opts: &PlanRunOptions, mut on_line: F) -> Result<PlanOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut outcome = PlanOutcome::default();
    if plan.steps.is_empty() {
        on_line(
            Stream::Stdout,
            "no automatic package steps for this distribution",
        );
        return Ok(outcome);
    }

    // Batch root steps into a single pkexec invocation so polkit only prompts once.
    if !opts.dry_run
        && opts.elevation == Elevation::Pkexec
        && plan.steps.iter().all(|step| step.needs_root)
    {
        return run_batched_pkexec(plan, opts, &mut on_line);
    }

    let total = plan.steps.len();
    for (index, step) in plan.steps.iter().enumerate() {
        on_line(
            Stream::Stdout,
            &format!("==> [{}/{}] {}", index + 1, total, step.label),
        );

        if opts.dry_run {
            let flags = match (step.needs_root, step.optional) {
                (true, true) => "root, optional",
                (true, false) => "root",
                (false, true) => "user, optional",
                (false, false) => "user",
            };
            on_line(
                Stream::Stdout,
                &format!("    [{flags}] sh -c {}", shell_quote(&step.script)),
            );
            outcome.completed.push(step.label.clone());
            continue;
        }

        let cmd = build_command(step, opts.elevation)?;
        let mut run_opts = RunOptions::default()
            .stall(Duration::from_secs(1800))
            .log_append();
        if let Some(path) = &opts.log_path {
            run_opts = run_opts.log(path);
        }
        if step.needs_root && matches!(opts.elevation, Elevation::Sudo) {
            run_opts = run_opts.inherit_stdin();
        }

        let result = runner::run(cmd, run_opts, &mut on_line)?;
        if !result.success() {
            let detail = result.tail_text(15);
            if step.optional {
                on_line(
                    Stream::Stderr,
                    &format!("optional step failed, continuing: {}", step.label),
                );
                outcome.failed_optional.push(step.label.clone());
                continue;
            }
            bail!("step '{}' failed\n{detail}", step.label);
        }
        outcome.completed.push(step.label.clone());
    }

    Ok(outcome)
}

fn batched_pkexec_script(plan: &PackagePlan) -> String {
    let total = plan.steps.len();
    let mut parts = Vec::with_capacity(total * 2);
    for (index, step) in plan.steps.iter().enumerate() {
        let header = format!("==> [{}/{}] {}", index + 1, total, step.label);
        parts.push(format!("printf '%s\\n' {}", shell_quote(&header)));
        if step.optional {
            let marker = format!("{OPT_FAIL_MARKER}{}", step.label);
            parts.push(format!(
                "({}) || printf '%s\\n' {} >&2",
                step.script,
                shell_quote(&marker)
            ));
        } else {
            parts.push(format!("({}) || exit $?", step.script));
        }
    }
    parts.join("\n")
}

fn run_batched_pkexec<F>(
    plan: &PackagePlan,
    opts: &PlanRunOptions,
    on_line: &mut F,
) -> Result<PlanOutcome>
where
    F: FnMut(Stream, &str),
{
    let script = batched_pkexec_script(plan);
    let mut cmd = Command::new("pkexec");
    cmd.arg("/bin/sh").arg("-c").arg(&script);
    let mut run_opts = RunOptions::default()
        .stall(Duration::from_secs(1800))
        .log_append();
    if let Some(path) = &opts.log_path {
        run_opts = run_opts.log(path);
    }

    let mut failed_optional = Vec::new();
    let result = runner::run(cmd, run_opts, |stream, line| {
        if let Some(label) = line.strip_prefix(OPT_FAIL_MARKER) {
            failed_optional.push(label.to_string());
            on_line(
                Stream::Stderr,
                &format!("optional step failed, continuing: {label}"),
            );
        } else {
            on_line(stream, line);
        }
    })?;

    if !result.success() {
        bail!("package installation failed\n{}", result.tail_text(15));
    }

    let completed = plan
        .steps
        .iter()
        .filter(|step| !failed_optional.contains(&step.label))
        .map(|step| step.label.clone())
        .collect();
    Ok(PlanOutcome {
        completed,
        failed_optional,
    })
}

fn build_command(step: &PackageStep, elevation: Elevation) -> Result<Command> {
    if !step.needs_root || elevation == Elevation::None {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&step.script);
        return Ok(cmd);
    }

    let cmd = match elevation {
        Elevation::Sudo => {
            let mut cmd = Command::new("sudo");
            cmd.arg("sh").arg("-c").arg(&step.script);
            cmd
        }
        Elevation::Pkexec => {
            let mut cmd = Command::new("pkexec");
            cmd.arg("/bin/sh").arg("-c").arg(&step.script);
            cmd
        }
        Elevation::None => unreachable!("handled above"),
    };
    Ok(cmd)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_for(text: &str) -> PackagePlan {
        plan_with(&OsRelease::parse(text), Some(true), Some(false))
    }

    fn scripts(plan: &PackagePlan) -> String {
        plan.steps
            .iter()
            .map(|step| step.script.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn arch_plan_uses_pacman_and_multilib() {
        let plan = plan_for("ID=cachyos\nID_LIKE=arch\n");
        assert_eq!(plan.family, Family::Arch);
        assert!(scripts(&plan).contains("pacman -Syu"));
        assert!(scripts(&plan).contains("winetricks"));
        assert!(scripts(&plan).contains("cabextract"));
        assert!(plan.notes.is_empty());

        let plan = plan_with(&OsRelease::parse("ID=arch\n"), Some(false), Some(false));
        assert!(plan.steps[0].script.contains("[multilib]"));
    }

    #[test]
    fn ubuntu_plan_installs_both_architectures() {
        let plan = plan_for("ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=debian\n");
        assert_eq!(plan.flavor, "ubuntu-24.4");
        let scripts = scripts(&plan);
        assert!(scripts.contains("dpkg --add-architecture i386"));
        assert!(scripts.contains("DEBIAN_FRONTEND=noninteractive"));
        assert!(scripts.contains("wine32:i386"));
        assert!(scripts.contains("p7zip-full"));
        assert!(scripts.contains("cabextract"));
    }

    #[test]
    fn ubuntu_2604_uses_7zip() {
        let plan = plan_for("ID=ubuntu\nVERSION_ID=\"26.04\"\n");
        let scripts = scripts(&plan);
        assert!(scripts.contains(" 7zip "));
        assert!(!scripts.contains("p7zip-full"));
    }

    #[test]
    fn ubuntu_derivatives_probe_7zip() {
        let plan = plan_for("ID=linuxmint\nVERSION_ID=\"22.1\"\nID_LIKE=\"ubuntu debian\"\n");
        assert_eq!(plan.flavor, "linuxmint");
        assert!(scripts(&plan).contains("apt-cache show 7zip"));
    }

    #[test]
    fn fedora_and_suse_plans() {
        let fedora = plan_for("ID=fedora\n");
        assert!(scripts(&fedora).contains("dnf install"));
        assert!(scripts(&fedora).contains("p7zip-plugins"));
        assert!(!fedora.steps[1].optional);

        let suse = plan_for("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n");
        assert!(scripts(&suse).contains("zypper --non-interactive refresh"));
        assert!(scripts(&suse).contains("zypper --non-interactive install"));
        assert!(scripts(&suse).contains("cabextract"));
    }

    #[test]
    fn unknown_and_bazzite_have_no_steps() {
        let unknown = plan_for("ID=void\n");
        assert!(unknown.steps.is_empty());
        assert!(!unknown.notes.is_empty());

        let bazzite = plan_for("ID=bazzite\nID_LIKE=fedora\n");
        assert!(bazzite.steps.is_empty());
        assert!(bazzite.notes.iter().any(|note| note.contains("immutable")));

        let silverblue = plan_with(
            &OsRelease::parse("ID=fedora\nPRETTY_NAME=\"Fedora Silverblue\"\n"),
            Some(true),
            Some(true),
        );
        assert!(silverblue.steps.is_empty());
        assert!(
            silverblue
                .notes
                .iter()
                .any(|note| note.contains("immutable"))
        );
    }

    #[test]
    fn batched_pkexec_combines_steps() {
        let plan = plan_for("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n");
        let script = batched_pkexec_script(&plan);
        assert!(script.contains("==> [1/3]"));
        assert!(script.contains("==> [3/3]"));
        assert!(script.contains("|| exit $?"));
        assert!(script.contains(OPT_FAIL_MARKER));
    }

    #[test]
    fn dry_run_does_not_execute() {
        let plan = plan_for("ID=arch\n");
        let opts = PlanRunOptions {
            dry_run: true,
            elevation: Elevation::None,
            log_path: None,
        };
        let mut lines = Vec::new();
        let outcome = run_plan(&plan, &opts, |_, line| lines.push(line.to_string())).unwrap();
        assert_eq!(outcome.completed.len(), plan.steps.len());
        assert!(lines.iter().any(|line| line.contains("pacman -Syu")));
    }
}
