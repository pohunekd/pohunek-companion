//! Installing the Pohunek release this build is pinned to: the asset names,
//! the download over HTTPS, the sha256 and attestation checks, and an
//! extractor that accepts only plain files and directories below one root.
//! No UI dependency; nothing runs until the caller asks for a step.

use std::collections::hash_map::RandomState;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;
use tracing::warn;

use crate::badge::{clip_display, sanitize};
use crate::config::SetupConfig;
use crate::setup::{ServiceCheck, run, service_check};

/// The `pohunek-client` git tag this build is pinned to (for example
/// `v0.35.2`), read from `Cargo.lock` at build time.
pub const CLIENT_TAG: &str = env!("POHUNEK_CLIENT_TAG");

/// The GitHub repository that publishes Pohunek releases; `gh attestation
/// verify` checks the archive against it.
const SOURCE_REPO: &str = "zajca/pohunek";
/// Target triple of the Linux daemon archive.
const LINUX_TARGET: &str = "x86_64-unknown-linux-gnu";
/// Oldest macOS the Homebrew formula supports.
const MACOS_MIN_MAJOR: u32 = 14;
/// Redirects followed for a release file (GitHub hands the file to a CDN).
const MAX_REDIRECTS: u32 = 5;
/// Bytes between two progress reports while downloading.
const PROGRESS_STEP: u64 = 4 << 20;
/// Longest checksum file read, in bytes.
const CHECKSUM_FILE_MAX: u64 = 4096;
const SHA256_HEX_LEN: usize = 64;
/// Leading hex digits of a hash shown on the screen; the full values are logged.
const SHA256_SHOWN: usize = 16;
const COPY_BUFFER: usize = 64 << 10;
const GH_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Below the state directory; holds one directory per running install.
const TEMP_DIR: &str = "install";
const TEMP_PREFIX: &str = "run-";
/// Name of the install script inside the unpacked archive.
const INSTALL_SCRIPT: &str = "packaging/install-daemon.sh";
/// The Homebrew formula of the macOS install.
const BREW_FORMULA: &str = "zajca/pohunek/pohunek";

/// The step of the install that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Download,
    Verify,
    Attest,
    Extract,
    Check,
    Install,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::Download => "Download",
            Self::Verify => "Checksum",
            Self::Attest => "Attestation",
            Self::Extract => "Unpack",
            Self::Check => "Pre-install check",
            Self::Install => "Install",
        }
    }
}

/// An install step that failed, with a short reason for the screen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{} failed: {message}", step.name())]
pub struct StepError {
    pub step: Step,
    pub message: String,
}

impl StepError {
    fn new(step: Step, message: impl Into<String>) -> Self {
        Self {
            step,
            message: message.into(),
        }
    }

    fn io(step: Step, what: &str, err: &io::Error) -> Self {
        Self::new(step, format!("{what}: {err}"))
    }
}

/// How this machine can get Pohunek.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    /// Linux x86_64: download, verify and run the release archive's installer.
    Release,
    /// macOS 14+ on Apple Silicon with Homebrew at this path.
    Brew { brew: PathBuf },
    /// macOS 14+ on Apple Silicon without Homebrew: manual steps only.
    Manual,
    /// Nothing is offered; the reason names the platform.
    Unsupported { reason: String },
}

/// Decides the plan from plain facts. `macos_version` is `sw_vers
/// -productVersion` on macOS, `brew` the located Homebrew.
pub fn plan_install(
    os: &str,
    arch: &str,
    macos_version: Option<&str>,
    brew: Option<PathBuf>,
) -> InstallPlan {
    match (os, arch) {
        ("linux", "x86_64") => InstallPlan::Release,
        ("macos", "aarch64") => match macos_version.and_then(macos_major) {
            Some(major) if major >= MACOS_MIN_MAJOR => match brew {
                Some(brew) => InstallPlan::Brew { brew },
                None => InstallPlan::Manual,
            },
            _ => InstallPlan::Unsupported {
                reason: format!(
                    "macOS {} or newer is required (this is {})",
                    MACOS_MIN_MAJOR,
                    macos_version.unwrap_or("an unknown version")
                ),
            },
        },
        _ => InstallPlan::Unsupported {
            reason: format!(
                "Pohunek releases cover Linux x86_64 and macOS on Apple Silicon, not {os} on {arch}"
            ),
        },
    }
}

fn macos_major(version: &str) -> Option<u32> {
    version.trim().split('.').next()?.parse().ok()
}

/// The plan for this machine; looks at `sw_vers` on macOS.
pub fn plan_for_host(brew: Option<PathBuf>) -> InstallPlan {
    let version = (std::env::consts::OS == "macos")
        .then(|| {
            Command::new("/usr/bin/sw_vers")
                .arg("-productVersion")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
        .flatten();
    plan_install(
        std::env::consts::OS,
        std::env::consts::ARCH,
        version.as_deref(),
        brew,
    )
}

/// The Pohunek release the installer fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The git tag, for example `v0.35.2`.
    pub tag: String,
    /// The tag without its `v`, as it appears in asset names.
    pub version: String,
}

impl Release {
    /// The release `pohunek-client` is pinned to.
    pub fn pinned() -> Self {
        // The build script accepts only tags that start with `v` and hold
        // plain characters, so this cannot fail.
        Self::from_tag(CLIENT_TAG).expect("build.rs validated the pinned tag")
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        let version = tag.strip_prefix('v')?;
        let plain = !version.is_empty()
            && version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
        plain.then(|| Self {
            tag: tag.to_owned(),
            version: version.to_owned(),
        })
    }

    /// The daemon archive for Linux x86_64.
    pub fn archive_name(&self) -> String {
        format!("pohunek-daemon-{}-{LINUX_TARGET}.tar.gz", self.version)
    }

    pub fn checksum_name(&self) -> String {
        format!("{}.sha256", self.archive_name())
    }

    /// The directory the archive unpacks into.
    pub fn root_dir_name(&self) -> String {
        format!("pohunek-daemon-{}-{LINUX_TARGET}", self.version)
    }

    /// `<base>/<tag>/<file>`.
    pub fn file_url(&self, base: &str, file: &str) -> String {
        format!("{}/{}/{file}", base.trim_end_matches('/'), self.tag)
    }
}

/// Accepts an `https` base URL, or `http` to a loopback host.
pub fn check_base_url(url: &str) -> Result<(), String> {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err("must start with https:// (or http:// for a loopback host)".to_owned());
    };
    if rest.chars().any(|c| c.is_whitespace() || c.is_control())
        || rest.contains(['@', '?', '#', '\\'])
    {
        return Err("must not hold whitespace, credentials, a query or a fragment".to_owned());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let host = if authority.starts_with('[') {
        authority.split_once(']').map_or(authority, |(h, _)| h)
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        return Err("has no host".to_owned());
    }
    if !https && !matches!(host, "127.0.0.1" | "localhost" | "[::1" | "::1") {
        return Err("http is allowed only for 127.0.0.1, localhost and [::1]".to_owned());
    }
    Ok(())
}

/// Whether `url` needs the downloader to refuse non-https redirects.
fn https_only(base: &str) -> bool {
    base.starts_with("https://")
}

/// The sha256 a `<hash>  <file>` checksum file states for `expected_name`.
pub fn parse_checksum(text: &str, expected_name: &str) -> Result<String, String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let line = lines.next().ok_or("the checksum file is empty")?;
    if lines.next().is_some() {
        return Err("the checksum file holds more than one entry".to_owned());
    }
    let mut parts = line.split_whitespace();
    let (Some(hash), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err("the checksum file is not `<sha256>  <file name>`".to_owned());
    };
    let name = name.strip_prefix('*').unwrap_or(name);
    if name != expected_name {
        return Err(format!(
            "the checksum file names {} instead of {expected_name}",
            clip_display(&sanitize(name), 80)
        ));
    }
    if hash.len() != SHA256_HEX_LEN || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the checksum is not 64 hexadecimal characters".to_owned());
    }
    Ok(hash.to_ascii_lowercase())
}

/// The sha256 of a file, streamed, as lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            hex.push_str(&format!("{byte:02x}"));
            hex
        }))
}

/// Compares the file's sha256 with the stated one; any difference refuses.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), StepError> {
    let actual =
        sha256_file(path).map_err(|err| StepError::io(Step::Verify, "cannot hash", &err))?;
    if actual == expected {
        Ok(())
    } else {
        warn!(%expected, %actual, "release archive sha256 mismatch");
        let short = |hash: &str| hash.chars().take(SHA256_SHOWN).collect::<String>();
        Err(StepError::new(
            Step::Verify,
            format!(
                "sha256 mismatch, the archive was refused. The release states {}..., the download \
                 is {}...",
                short(expected),
                short(&actual)
            ),
        ))
    }
}

/// A private directory that is removed, with everything in it, when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A fresh directory (mode 0700) below the app's state directory, which
    /// is on disk and, unlike `/tmp` on hardened systems, not mounted `noexec`:
    /// the unpacked `pohunek` is executed from it.
    pub fn create(stale_after: Duration) -> io::Result<Self> {
        let base = crate::layout::state_dir()
            .map_err(|err| io::Error::other(err.to_string()))?
            .join(TEMP_DIR);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)?;
        sweep_stale(&base, stale_after);
        for _ in 0..32 {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u32(std::process::id());
            hasher.write_u128(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos()),
            );
            let path = base.join(format!("{TEMP_PREFIX}{:016x}", hasher.finish()));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no unused temporary directory name",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Removes install directories older than `stale_after` that an install which
/// was killed left behind. Only plain directories named like the ones
/// [`TempDir::create`] makes are touched.
fn sweep_stale(base: &Path, stale_after: Duration) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > stale_after);
        let ours = entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX);
        if ours
            && meta.is_dir()
            && old
            && let Err(err) = fs::remove_dir_all(entry.path())
        {
            warn!(path = %entry.path().display(), error = %err, "remove a stale install directory");
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.path) {
            warn!(path = %self.path.display(), error = %err, "remove the install temporary directory");
        }
    }
}

/// Downloads `url` into a new private file. `max_bytes` bounds the body;
/// `report` receives the byte count every few MiB.
pub fn download(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    timeout: Duration,
    https_only: bool,
    report: &dyn Fn(u64),
) -> Result<u64, StepError> {
    let fail = |message: String| StepError::new(Step::Download, message);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(https_only)
        .max_redirects(MAX_REDIRECTS)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|err| fail(format!("{}: {err}", clip_display(&sanitize(url), 120))))?;
    let mut reader = response.body_mut().with_config().limit(max_bytes).reader();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|err| StepError::io(Step::Download, "cannot create the file", &err))?;
    let mut buffer = vec![0u8; COPY_BUFFER];
    let (mut total, mut reported) = (0u64, 0u64);
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| fail(format!("the transfer broke off: {err}")))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|err| StepError::io(Step::Download, "cannot write the file", &err))?;
        total += read as u64;
        if total - reported >= PROGRESS_STEP {
            reported = total;
            report(total);
        }
    }
    file.sync_all()
        .map_err(|err| StepError::io(Step::Download, "cannot write the file", &err))?;
    Ok(total)
}

/// The outcome of the optional provenance check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attestation {
    Verified,
    /// Not checked, with the reason shown to the owner.
    Skipped(String),
}

/// `gh attestation verify <file> --repo zajca/pohunek` when `gh` is usable.
/// A failing verification refuses the install; a missing or signed-out `gh`
/// only skips the check and says so.
pub fn verify_attestation(
    gh: Option<&Path>,
    file: &Path,
    max_chars: usize,
) -> Result<Attestation, StepError> {
    let Some(gh) = gh else {
        return Ok(Attestation::Skipped(
            "the gh command is not installed".to_owned(),
        ));
    };
    let signed_in = run(gh, &["auth", "status"], GH_PROBE_TIMEOUT).is_ok_and(|out| out.success);
    if !signed_in {
        return Ok(Attestation::Skipped(
            "gh is not signed in to GitHub".to_owned(),
        ));
    }
    let file_arg = file.to_string_lossy();
    let out = run(
        gh,
        &["attestation", "verify", &file_arg, "--repo", SOURCE_REPO],
        GH_PROBE_TIMEOUT * 3,
    )
    .map_err(|err| StepError::new(Step::Attest, err))?;
    if out.success {
        Ok(Attestation::Verified)
    } else {
        let text = if out.stderr.trim().is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        Err(StepError::new(
            Step::Attest,
            clip_display(&sanitize(text), max_chars),
        ))
    }
}

/// What an archive entry becomes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryPlan {
    Dir,
    File { executable: bool },
}

/// Decides one archive entry. Only regular files and directories whose path
/// is `root/...` made of plain components are accepted: symlinks, hard links,
/// devices, absolute paths, `.` and `..` are refused.
pub fn plan_entry(
    root: &str,
    path: &Path,
    kind: EntryType,
    mode: u32,
) -> Result<(PathBuf, EntryPlan), String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            _ => {
                return Err(format!(
                    "the path {} is not a plain relative path",
                    clip_display(&sanitize(&path.to_string_lossy()), 80)
                ));
            }
        }
    }
    if parts.first().is_none_or(|first| *first != root) {
        return Err(format!(
            "{} lies outside {root}/",
            clip_display(&sanitize(&path.to_string_lossy()), 80)
        ));
    }
    let relative: PathBuf = parts.iter().collect();
    match kind {
        EntryType::Directory => Ok((relative, EntryPlan::Dir)),
        EntryType::Regular => Ok((
            relative,
            EntryPlan::File {
                executable: mode & 0o111 != 0,
            },
        )),
        other => Err(format!(
            "{} is not a plain file or directory (entry type {:?})",
            clip_display(&sanitize(&path.to_string_lossy()), 80),
            other
        )),
    }
}

/// Unpacks the gzip tar `archive` into `dest`; returns the unpacked root.
/// Every entry goes through [`plan_entry`]; files are created exclusively and
/// with owner-only write permission, so nothing outside `dest/root` is
/// touched and no entry can overwrite another.
pub fn extract(
    archive: &Path,
    dest: &Path,
    root: &str,
    max_bytes: u64,
    max_entries: u64,
) -> Result<PathBuf, StepError> {
    let fail = |message: String| StepError::new(Step::Extract, message);
    let file = File::open(archive)
        .map_err(|err| StepError::io(Step::Extract, "cannot open the archive", &err))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|err| fail(format!("not a readable tar.gz archive: {err}")))?;
    let (mut count, mut written) = (0u64, 0u64);
    for entry in entries {
        let mut entry = entry.map_err(|err| fail(format!("broken archive entry: {err}")))?;
        count += 1;
        if count > max_entries {
            return Err(fail(format!(
                "the archive holds more than {max_entries} entries"
            )));
        }
        let path = entry
            .path()
            .map_err(|err| fail(format!("unreadable entry path: {err}")))?
            .into_owned();
        let mode = entry.header().mode().unwrap_or(0);
        let (relative, plan) =
            plan_entry(root, &path, entry.header().entry_type(), mode).map_err(fail)?;
        let target = dest.join(relative);
        match plan {
            EntryPlan::Dir => {
                create_dir(&target).map_err(|err| {
                    StepError::io(Step::Extract, "cannot create a directory", &err)
                })?;
            }
            EntryPlan::File { executable } => {
                let size = entry.size();
                written = written.saturating_add(size);
                if written > max_bytes {
                    return Err(fail(format!(
                        "the unpacked archive would exceed {} MiB",
                        max_bytes >> 20
                    )));
                }
                if let Some(parent) = target.parent() {
                    create_dir(parent).map_err(|err| {
                        StepError::io(Step::Extract, "cannot create a directory", &err)
                    })?;
                }
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(if executable { 0o755 } else { 0o644 })
                    .open(&target)
                    .map_err(|err| StepError::io(Step::Extract, "cannot create a file", &err))?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|err| StepError::io(Step::Extract, "cannot write a file", &err))?;
                if copied != size {
                    return Err(fail("the archive ends inside a file".to_owned()));
                }
            }
        }
    }
    Ok(dest.join(root))
}

fn create_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o755).create(path)
}

/// A downloaded, verified and unpacked release waiting for the owner's
/// confirmation. Dropping it removes the temporary directory.
#[derive(Debug)]
pub struct Prepared {
    pub release: Release,
    pub archive_name: String,
    pub url: String,
    pub sha256: String,
    pub attestation: Attestation,
    /// The `pohunek` CLI inside the unpacked archive.
    pub cli: PathBuf,
    pub script: PathBuf,
    pub check: ServiceCheck,
    _temp: TempDir,
}

impl Prepared {
    /// What the owner confirms: the evidence of the verification, the plan of
    /// the pre-install check, and the exact command.
    pub fn preview(&self) -> String {
        let attestation = match &self.attestation {
            Attestation::Verified => {
                format!("verified with gh attestation verify (repo {SOURCE_REPO})")
            }
            Attestation::Skipped(why) => format!("not checked ({why})"),
        };
        format!(
            "Downloaded {} from {}\nsha256 {} matches the release checksum file.\nAttestation: {attestation}.\n\n$ ./{INSTALL_SCRIPT}\n\n{}",
            self.archive_name,
            self.url,
            self.sha256,
            self.check
                .summary(crate::setup::SetupAction::InstallRelease),
        )
    }
}

/// Downloads, verifies and unpacks the release and runs the unpacked CLI's
/// read-only `service check`. `progress` receives one line per step. The
/// temporary directory is removed on every failure path (it is a guard).
pub fn prepare_release(
    config: &SetupConfig,
    release: &Release,
    gh: Option<&Path>,
    progress: &dyn Fn(String),
) -> Result<Prepared, StepError> {
    let temp =
        TempDir::create(Duration::from_secs(config.stale_install_max_age_secs)).map_err(|err| {
            StepError::io(Step::Download, "cannot create a temporary directory", &err)
        })?;
    let base = &config.release_base_url;
    let strict = https_only(base);
    let timeout = config.download_timeout();
    let archive_name = release.archive_name();
    let checksum_name = release.checksum_name();
    let archive_url = release.file_url(base, &archive_name);
    let checksum_url = release.file_url(base, &checksum_name);
    let archive_path = temp.path().join(&archive_name);
    let checksum_path = temp.path().join(&checksum_name);

    progress(format!("Downloading {checksum_name}"));
    download(
        &checksum_url,
        &checksum_path,
        CHECKSUM_FILE_MAX,
        timeout,
        strict,
        &|_| {},
    )?;
    let checksum_text = fs::read_to_string(&checksum_path)
        .map_err(|err| StepError::io(Step::Verify, "cannot read the checksum file", &err))?;
    let expected = parse_checksum(&checksum_text, &archive_name)
        .map_err(|message| StepError::new(Step::Verify, message))?;

    progress(format!("Downloading {archive_name}"));
    download(
        &archive_url,
        &archive_path,
        config.download_max_mib.saturating_mul(1 << 20),
        timeout,
        strict,
        &|bytes| progress(format!("Downloading {archive_name}: {} MiB", bytes >> 20)),
    )?;

    progress("Verifying the sha256".to_owned());
    verify_sha256(&archive_path, &expected)?;

    progress("Verifying the release attestation".to_owned());
    let attestation = verify_attestation(gh, &archive_path, config.message_max_chars)?;

    progress("Unpacking".to_owned());
    let unpack_dir = temp.path().join("unpacked");
    create_dir(&unpack_dir)
        .map_err(|err| StepError::io(Step::Extract, "cannot create a directory", &err))?;
    let root = extract(
        &archive_path,
        &unpack_dir,
        &release.root_dir_name(),
        config.extract_max_mib.saturating_mul(1 << 20),
        config.extract_max_entries,
    )?;
    let cli = root.join("pohunek");
    let script = root.join(INSTALL_SCRIPT);
    for needed in [&cli, &script] {
        if !needed.is_file() {
            return Err(StepError::new(
                Step::Extract,
                format!(
                    "the archive lacks {}",
                    needed.strip_prefix(&root).unwrap_or(needed).display()
                ),
            ));
        }
    }

    progress("Running the pre-install check".to_owned());
    let check = service_check(&cli, config.probe_timeout(), config.message_max_chars)
        .map_err(|err| StepError::new(Step::Check, err.to_string()))?;
    Ok(Prepared {
        release: release.clone(),
        archive_name,
        url: archive_url,
        sha256: expected,
        attestation,
        cli,
        script,
        check,
        _temp: temp,
    })
}

/// Runs the unpacked release's `packaging/install-daemon.sh`, without any
/// flag. `Ok` is a one-line outcome.
pub fn run_release_install(
    prepared: &Prepared,
    timeout: Duration,
    max_chars: usize,
) -> Result<String, StepError> {
    let out =
        run(&prepared.script, &[], timeout).map_err(|err| StepError::new(Step::Install, err))?;
    if out.success {
        Ok(format!(
            "Pohunek {} is installed.",
            prepared.release.version
        ))
    } else {
        let text = if out.stderr.trim().is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        Err(StepError::new(
            Step::Install,
            clip_display(&sanitize(text), max_chars),
        ))
    }
}

/// The command line the macOS install shows and runs.
pub fn brew_command_line() -> String {
    format!("brew install {BREW_FORMULA}")
}

/// Runs `brew install zajca/pohunek/pohunek`.
pub fn run_brew_install(
    brew: &Path,
    timeout: Duration,
    max_chars: usize,
) -> Result<String, StepError> {
    let out = run(brew, &["install", BREW_FORMULA], timeout)
        .map_err(|err| StepError::new(Step::Install, err))?;
    if out.success {
        Ok("Homebrew installed Pohunek.".to_owned())
    } else {
        let text = if out.stderr.trim().is_empty() {
            &out.stdout
        } else {
            &out.stderr
        };
        Err(StepError::new(
            Step::Install,
            clip_display(&sanitize(text), max_chars),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "5b715b92f05fa9a54a4856452f98c75456f1042436d3dfccdcfa0b5f1da6225b";

    #[test]
    fn release_names_follow_the_pinned_tag() {
        let release = Release::from_tag("v0.35.2").unwrap();
        assert_eq!(
            release.archive_name(),
            "pohunek-daemon-0.35.2-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            release.file_url("https://example.org/dl/", &release.checksum_name()),
            "https://example.org/dl/v0.35.2/pohunek-daemon-0.35.2-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
        assert_eq!(Release::from_tag("0.35.2"), None);
        assert_eq!(Release::from_tag("v0/../x"), None);
        assert_eq!(Release::from_tag("v"), None);
        assert_eq!(Release::pinned().tag, CLIENT_TAG);
    }

    #[test]
    fn checksum_file_must_name_the_archive_with_a_full_hash() {
        let name = "a.tar.gz";
        assert_eq!(
            parse_checksum(&format!("{HASH}  {name}\n"), name).as_deref(),
            Ok(HASH)
        );
        assert_eq!(
            parse_checksum(&format!("{} *{name}", HASH.to_uppercase()), name).as_deref(),
            Ok(HASH)
        );
        assert!(parse_checksum(&format!("{HASH}  other.tar.gz"), name).is_err());
        assert!(parse_checksum(&format!("{}  {name}", &HASH[..60]), name).is_err());
        assert!(parse_checksum(&format!("{}zz  {name}", &HASH[..62]), name).is_err());
        assert!(parse_checksum(&format!("{HASH}  {name}\n{HASH}  {name}"), name).is_err());
        assert!(parse_checksum("", name).is_err());
    }

    #[test]
    fn hash_mismatch_is_refused() {
        let dir = TempDir::create(Duration::from_secs(3600)).unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"abc").unwrap();
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(verify_sha256(&file, good), Ok(()));
        let err = verify_sha256(&file, HASH).unwrap_err();
        assert_eq!(err.step, Step::Verify);
        assert!(err.message.contains("mismatch"));
    }

    #[test]
    fn base_url_must_be_https_or_loopback_http() {
        assert!(check_base_url("https://github.com/zajca/pohunek/releases/download").is_ok());
        assert!(check_base_url("http://127.0.0.1:8080/dl").is_ok());
        assert!(check_base_url("http://localhost/dl").is_ok());
        assert!(check_base_url("http://[::1]:9/dl").is_ok());
        assert!(check_base_url("http://example.org/dl").is_err());
        assert!(check_base_url("ftp://example.org").is_err());
        assert!(check_base_url("https://user@example.org").is_err());
        assert!(check_base_url("https://").is_err());
        assert!(check_base_url("https://example.org/a b").is_err());
    }

    #[test]
    fn archive_entries_are_limited_to_plain_files_below_the_root() {
        let root = "r";
        let plan = |p: &str, kind| plan_entry(root, Path::new(p), kind, 0o755);
        assert_eq!(
            plan("r/bin/x", EntryType::Regular),
            Ok((
                PathBuf::from("r/bin/x"),
                EntryPlan::File { executable: true }
            ))
        );
        assert_eq!(
            plan("r/", EntryType::Directory),
            Ok((PathBuf::from("r"), EntryPlan::Dir))
        );
        for bad in ["/etc/passwd", "r/../x", "../r/x", "./r/x", "other/x"] {
            assert!(plan(bad, EntryType::Regular).is_err(), "{bad}");
        }
        for kind in [
            EntryType::Symlink,
            EntryType::Link,
            EntryType::Char,
            EntryType::Block,
            EntryType::Fifo,
            EntryType::GNUSparse,
        ] {
            assert!(plan("r/x", kind).is_err(), "{kind:?}");
        }
    }

    #[test]
    fn extraction_refuses_traversal_and_links_and_unpacks_files() {
        let build = |entries: &[(&str, EntryType, &[u8], Option<&str>)]| -> Vec<u8> {
            let mut builder = tar::Builder::new(Vec::new());
            for (path, kind, data, link) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(*kind);
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                if let Some(link) = link {
                    header.set_link_name(link).unwrap();
                }
                // `set_path` rejects `..`; write the raw name like a hostile archive would.
                let name = path.as_bytes();
                header.as_old_mut().name[..name.len()].copy_from_slice(name);
                header.set_cksum();
                builder.append(&header, *data).unwrap();
            }
            let tar = builder.into_inner().unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&tar).unwrap();
            gz.finish().unwrap()
        };
        let dir = TempDir::create(Duration::from_secs(3600)).unwrap();
        let run_case = |name: &str, bytes: Vec<u8>| {
            let archive = dir.path().join(format!("{name}.tar.gz"));
            fs::write(&archive, bytes).unwrap();
            let dest = dir.path().join(name);
            fs::create_dir(&dest).unwrap();
            extract(&archive, &dest, "r", 1 << 20, 100)
        };
        let ok = run_case(
            "ok",
            build(&[
                ("r/", EntryType::Directory, b"", None),
                ("r/hello", EntryType::Regular, b"hi", None),
            ]),
        )
        .unwrap();
        assert_eq!(fs::read(ok.join("hello")).unwrap(), b"hi");
        assert!(
            run_case(
                "up",
                build(&[("r/../../evil", EntryType::Regular, b"x", None)])
            )
            .is_err()
        );
        assert!(
            run_case(
                "abs",
                build(&[("/tmp/evil", EntryType::Regular, b"x", None)])
            )
            .is_err()
        );
        assert!(
            run_case(
                "sym",
                build(&[("r/l", EntryType::Symlink, b"", Some("/etc"))])
            )
            .is_err()
        );
        assert!(
            run_case(
                "big",
                build(&[("r/f", EntryType::Regular, &vec![0u8; 2 << 20], None)])
            )
            .is_err()
        );
        assert!(!dir.path().join("evil").exists());
    }

    #[test]
    fn plans_cover_supported_platforms_only() {
        let brew = Some(PathBuf::from("/opt/homebrew/bin/brew"));
        assert_eq!(
            plan_install("linux", "x86_64", None, None),
            InstallPlan::Release
        );
        assert!(matches!(
            plan_install("linux", "aarch64", None, None),
            InstallPlan::Unsupported { .. }
        ));
        assert!(matches!(
            plan_install("macos", "aarch64", Some("14.6.1"), brew.clone()),
            InstallPlan::Brew { .. }
        ));
        assert_eq!(
            plan_install("macos", "aarch64", Some("15.0"), None),
            InstallPlan::Manual
        );
        assert!(matches!(
            plan_install("macos", "aarch64", Some("13.7"), brew.clone()),
            InstallPlan::Unsupported { .. }
        ));
        assert!(matches!(
            plan_install("macos", "x86_64", Some("14.0"), brew),
            InstallPlan::Unsupported { .. }
        ));
    }
}
