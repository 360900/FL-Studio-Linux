//! Per-distribution dependency installation plans.
//!
//! The FL Studio installer uses its own Wine build, but the host needs the
//! regular 64-bit Wine libraries, winetricks and the archive tools winetricks
//! downloads fonts with. The private build is WoW64, so no 32-bit libraries
//! are needed. Commands are rendered as shell snippets so they can be shown in
//! a dry run, logged, and executed with sudo or pkexec.

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
}

#[derive(Debug, Clone)]
pub struct PackagePlan {
    pub family: Family,
    pub flavor: String,
    pub steps: Vec<PackageStep>,
    pub notes: Vec<String>,
}

pub fn plan(os: &OsRelease) -> PackagePlan {
    plan_with(os, None)
}

/// Signs of an image-based system whose package manager cannot change the
/// running root: rpm-ostree, transactional-update, SteamOS's read-only mode
/// or a read-only `/usr`.
fn is_immutable_host() -> bool {
    [
        "/run/ostree-booted",
        "/usr/sbin/transactional-update",
        "/usr/bin/steamos-readonly",
    ]
    .iter()
    .any(|path| Path::new(path).exists())
        || std::fs::read_to_string("/proc/self/mountinfo").is_ok_and(|text| usr_is_read_only(&text))
}

/// Whether the mount holding `/usr` is read-only, from mountinfo text.
fn usr_is_read_only(mountinfo: &str) -> bool {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let mount = fields.nth(4)?;
            let options = fields.next()?;
            let covers = mount == "/" || mount == "/usr";
            covers.then_some((mount.len(), options))
        })
        // The deepest mount wins, and the last of equal depth is on top.
        .max_by_key(|(depth, _)| *depth)
        .is_some_and(|(_, options)| options.split(',').any(|option| option == "ro"))
}

fn immutable_distro(os: &OsRelease) -> bool {
    const IDS: &[&str] = &[
        "bazzite",
        "steamos",
        "chimeraos",
        "microos",
        "aeon",
        "kalpa",
    ];
    let id = os.id.to_ascii_lowercase();
    IDS.iter().any(|name| id.contains(name))
        || os.id_like.iter().any(|like| like.contains("microos"))
}

fn plan_with(os: &OsRelease, immutable_host: Option<bool>) -> PackagePlan {
    let family = os.family();
    let id = os.id.to_ascii_lowercase();
    let mut notes = Vec::new();

    if id == "manjaro" {
        notes.push(
            "Manjaro is not officially supported. The Arch plan may still work, but expect rough edges."
                .to_string(),
        );
    }
    if immutable_distro(os) || immutable_host.unwrap_or_else(is_immutable_host) {
        notes.push(format!(
            "{} is an immutable distribution; automatic dependency installation is disabled. Install winetricks, cabextract, 7z and Wine's libraries with its own tools (rpm-ostree, transactional-update, a container) first.",
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
        Family::Arch => arch_plan(),
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
        Family::Enterprise => {
            notes.push(
                "RHEL-based distributions ship no Wine. Install wine, winetricks, cabextract, 7z, tar, xz and unzip manually, for example from EPEL."
                    .to_string(),
            );
            ("enterprise".to_string(), Vec::new())
        }
        Family::Unknown if id == "nixos" => {
            notes.push(
                "NixOS cannot run the prebuilt Wine build directly. Enable programs.nix-ld with Wine's libraries, and install winetricks, cabextract and p7zip."
                    .to_string(),
            );
            ("nixos".to_string(), Vec::new())
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

/// Arch's `wine` keeps TLS, audio and Vulkan as optional dependencies, but
/// the private build needs them, so they are listed explicitly.
fn arch_plan() -> (String, Vec<PackageStep>) {
    let steps = vec![PackageStep::root(
        "Install Wine libraries and installer tools",
        pacman_install(
            "-Syu",
            "wine winetricks cabextract wget curl 7zip unzip tar xz pciutils icoutils gnutls alsa-lib libpulse vulkan-icd-loader libxcomposite libxinerama",
        ),
    )];
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
                "DEBIAN_FRONTEND=noninteractive apt-get install -y wine wine64 winetricks cabextract wget curl {archive} tar xz-utils pciutils unzip icoutils"
            ),
        )
    } else {
        // Ubuntu derivatives carry their own VERSION_ID scheme, so detect the
        // archive package at install time. Older repos also ship a `7zip`
        // that only provides `7zz`, so `p7zip-full` wins whenever it exists.
        (
            os.id.clone(),
            "archive=7zip; apt-cache show p7zip-full >/dev/null 2>&1 && archive=p7zip-full; DEBIAN_FRONTEND=noninteractive apt-get install -y wine wine64 winetricks cabextract wget curl \"$archive\" tar xz-utils pciutils unzip icoutils".to_string(),
        )
    };
    let steps = vec![
        PackageStep::root("Refresh package lists", "apt-get update"),
        PackageStep::root("Install Wine libraries and installer tools", install_script),
    ];
    (flavor, steps)
}

fn fedora_plan() -> (String, Vec<PackageStep>) {
    (
        "fedora".to_string(),
        vec![PackageStep::root(
            "Install Wine libraries and installer tools",
            "dnf install -y wine winetricks cabextract wget curl p7zip p7zip-plugins tar xz pciutils unzip icoutils",
        )],
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
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
}

/// The prefix, menu entries and config belong to the user. Run as root they
/// would land in root's home, or leave root-owned files in the user's.
pub fn ensure_not_root() -> Result<()> {
    if is_root() {
        bail!(
            "run FL Studio Linux as your normal user, not as root or with sudo; it asks for administrator rights itself when it installs system packages"
        );
    }
    Ok(())
}

/// Advice when polkit refused or could not ask, e.g. without an
/// authentication agent on a bare window manager.
fn pkexec_failure(result: &runner::RunResult) -> Option<String> {
    let lines = result.stderr_tail.iter().chain(&result.stdout_tail);
    let reason = lines
        .filter_map(|line| line.split_once("Error executing command as another user:"))
        .map(|(_, reason)| reason.trim().to_string())
        .next_back()?;
    Some(format!(
        "could not get administrator rights through polkit ({reason}). Run `fl-studio-linux deps` in a terminal to install the packages with sudo, then install again"
    ))
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
        if !result.success()
            && step.needs_root
            && opts.elevation == Elevation::Pkexec
            && let Some(advice) = pkexec_failure(&result)
        {
            bail!("{advice}");
        }
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
        if let Some(advice) = pkexec_failure(&result) {
            bail!("{advice}");
        }
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
        plan_with(&OsRelease::parse(text), Some(false))
    }

    fn scripts(plan: &PackagePlan) -> String {
        plan.steps
            .iter()
            .map(|step| step.script.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn arch_plan_uses_pacman_without_multilib() {
        let plan = plan_for("ID=cachyos\nID_LIKE=arch\n");
        assert_eq!(plan.family, Family::Arch);
        let scripts = scripts(&plan);
        assert!(scripts.contains("pacman -Syu"));
        assert!(scripts.contains("winetricks"));
        assert!(scripts.contains("cabextract"));
        assert!(scripts.contains("gnutls"));
        assert!(scripts.contains("vulkan-icd-loader"));
        assert!(!scripts.contains("multilib"));
        assert!(!scripts.contains("lib32"));
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn ubuntu_plan_needs_no_i386() {
        let plan = plan_for("ID=ubuntu\nVERSION_ID=\"24.04\"\nID_LIKE=debian\n");
        assert_eq!(plan.flavor, "ubuntu-24.4");
        let scripts = scripts(&plan);
        assert!(!scripts.contains("i386"));
        assert!(scripts.contains("DEBIAN_FRONTEND=noninteractive"));
        assert!(scripts.contains("wine64"));
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
    fn ubuntu_derivatives_prefer_p7zip_full() {
        let plan = plan_for("ID=linuxmint\nVERSION_ID=\"22.1\"\nID_LIKE=\"ubuntu debian\"\n");
        assert_eq!(plan.flavor, "linuxmint");
        assert!(scripts(&plan).contains("apt-cache show p7zip-full"));
    }

    #[test]
    fn fedora_and_suse_plans() {
        let fedora = plan_for("ID=fedora\n");
        assert!(scripts(&fedora).contains("dnf install"));
        assert!(scripts(&fedora).contains("p7zip-plugins"));
        assert!(!scripts(&fedora).contains("i686"));

        let suse = plan_for("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n");
        assert!(scripts(&suse).contains("zypper --non-interactive refresh"));
        assert!(scripts(&suse).contains("zypper --non-interactive install"));
        assert!(scripts(&suse).contains("cabextract"));
        assert!(!scripts(&suse).contains("32bit"));
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
        );
        assert!(silverblue.steps.is_empty());
        assert!(
            silverblue
                .notes
                .iter()
                .any(|note| note.contains("immutable"))
        );

        for text in [
            "ID=steamos\nID_LIKE=arch\n",
            "ID=chimeraos\nID_LIKE=arch\n",
            "ID=\"opensuse-microos\"\nID_LIKE=\"suse opensuse opensuse-tumbleweed microos\"\n",
        ] {
            assert!(plan_for(text).steps.is_empty(), "{text}");
        }
    }

    #[test]
    fn enterprise_and_nixos_get_manual_notes() {
        let rocky = plan_for("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n");
        assert!(rocky.steps.is_empty());
        assert!(rocky.notes.iter().any(|note| note.contains("EPEL")));
        let nixos = plan_for("ID=nixos\n");
        assert!(nixos.steps.is_empty());
        assert!(nixos.notes.iter().any(|note| note.contains("nix-ld")));
    }

    #[test]
    fn detects_a_read_only_usr() {
        let rw = "22 1 0:21 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw\n";
        assert!(!usr_is_read_only(rw));
        let ro_root = "22 1 0:21 / / ro,relatime shared:1 - ext4 /dev/sda1 ro\n";
        assert!(usr_is_read_only(ro_root));
        let ro_usr = format!("{rw}23 22 0:22 / /usr ro,relatime - erofs overlay ro\n");
        assert!(usr_is_read_only(&ro_usr));
    }

    #[test]
    fn batched_pkexec_combines_steps() {
        let mut plan = plan_for("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n");
        plan.steps.push(PackageStep {
            optional: true,
            ..PackageStep::root("Optional extra", "true")
        });
        let script = batched_pkexec_script(&plan);
        assert!(script.contains("==> [1/3]"));
        assert!(script.contains("==> [3/3]"));
        assert!(script.contains("|| exit $?"));
        assert!(script.contains(OPT_FAIL_MARKER));
    }

    #[test]
    fn explains_polkit_failures() {
        use std::os::unix::process::ExitStatusExt;
        let result = |line: &str| runner::RunResult {
            status: std::process::ExitStatus::from_raw(127 << 8),
            stalled: false,
            cancelled: false,
            stdout_tail: Vec::new(),
            stderr_tail: vec![line.to_string()],
        };
        let advice = pkexec_failure(&result(
            "Error executing command as another user: No authentication agent found.",
        ))
        .unwrap();
        assert!(advice.contains("No authentication agent found"));
        assert!(advice.contains("fl-studio-linux deps"));
        assert!(pkexec_failure(&result("sh: apt-get: not found")).is_none());
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
