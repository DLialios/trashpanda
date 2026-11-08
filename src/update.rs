//! Updates from the project's GitHub releases.
//!
//! A release holds a build per platform, a minisign signature for each, and
//! `latest.json`, which names the version and each platform's build. A
//! signature's trusted comment, `trashpanda <version> <platform>`, is signed
//! along with the build. A build is installed only if it is signed with the
//! key compiled in here, for this platform, as the version `latest.json`
//! offers, and that version is newer than this one. Access to the GitHub
//! account alone can hold updates back, but cannot ship one.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use minisign_verify::{PublicKey, Signature};

use crate::log;

/// GitHub redirects `<RELEASES>/latest/download/<file>` to that file in the
/// newest published release.
pub const RELEASES: &str = "https://github.com/DLialios/trashpanda/releases";
/// The public half of the release signing key.
const PUBLIC_KEY: &str = include_str!("../minisign.pub");
/// How the name of the file a new build is written to, next to the running
/// program, starts. Each update attempt writes to a file of its own.
pub const STAGED: &str = ".trashpanda-update";

/// A `major.minor.patch` version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let number = |part: &str| part.bytes().all(|b| b.is_ascii_digit()).then(|| part.parse().ok()).flatten();
        match text.split('.').map(number).collect::<Option<Vec<u64>>>().as_deref() {
            Some(&[major, minor, patch]) => Ok(Version(major, minor, patch)),
            _ => bail!("{text:?} is not a version"),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version of this build.
pub fn current() -> Version {
    env!("CARGO_PKG_VERSION").parse().expect("the package version is major.minor.patch")
}

/// A release newer than this build, and the name of its build for this platform.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    /// The release's page on GitHub, with its notes and files.
    pub page: String,
    asset: String,
}

/// The program's folder cannot be written to, so the update has to be
/// downloaded by hand from `page`.
#[derive(Debug)]
pub struct Unwritable {
    pub dir: PathBuf,
    pub page: String,
    error: io::Error,
}

impl fmt::Display for Unwritable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "cannot write to {} ({}); download the new version by hand from {}",
               self.dir.display(), self.error, self.page)
    }
}

impl std::error::Error for Unwritable {}

/// Where updates come from, and what this build is.
pub struct Updater {
    releases: String,
    platform: String,
    current: Version,
    key: PublicKey,
    agent: ureq::Agent,
}

impl Updater {
    pub fn new() -> Self {
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        Self::with(RELEASES, &platform, current(), PUBLIC_KEY).expect("minisign.pub holds a minisign public key")
    }

    /// An updater for the releases at `releases`, signed with `public_key`
    /// (the text of a `minisign.pub`).
    pub fn with(releases: &str, platform: &str, current: Version, public_key: &str) -> Result<Self> {
        let key = PublicKey::decode(public_key).map_err(|error| anyhow!("bad public key: {error}"))?;
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .user_agent(concat!("trashpanda/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Self { releases: releases.trim_end_matches('/').into(), platform: platform.into(), current, key, agent })
    }

    /// The newest release, if it is newer than this build.
    pub fn check(&self) -> Result<Option<Release>> {
        let failed = "could not check for updates";
        let latest = self.get("latest/download/latest.json", 64 << 10).context(failed)?;
        let latest = String::from_utf8_lossy(&latest);
        log::write(format!("update: latest.json: {}", log::clip(&latest, 4 << 10)));
        let release = self.parse_latest(&latest).context(failed)?;
        log::write(format!("update: this is {} for {}; newer release: {release:?}", self.current, self.platform));
        Ok(release)
    }

    /// The release `latest.json` describes,
    /// `{"version": "0.3.0", "assets": {"linux-x86_64": "trashpanda-linux-x86_64", ...}}`,
    /// or None if it is not newer than this build.
    pub fn parse_latest(&self, json: &str) -> Result<Option<Release>> {
        let value: serde_json::Value = serde_json::from_str(json).context("latest.json is not JSON")?;
        let version: Version = value["version"].as_str().context("latest.json has no version")?.parse()?;
        if version <= self.current {
            return Ok(None);
        }
        let asset = value["assets"][&self.platform].as_str()
            .with_context(|| format!("version {version} has no build for {}", self.platform))?;
        let safe = |b: u8| b.is_ascii_alphanumeric() || b"._-".contains(&b);
        ensure!(!asset.starts_with('.') && asset.bytes().all(safe), "latest.json names an odd file, {asset:?}");
        let page = format!("{}/tag/v{version}", self.releases);
        Ok(Some(Release { version, page, asset: asset.into() }))
    }

    /// Download the release's build and its signature, and return the build if
    /// the signature holds.
    pub fn download(&self, release: &Release) -> Result<Vec<u8>> {
        let path = format!("download/v{}/{}", release.version, release.asset);
        let build = self.get(&path, 256 << 20).with_context(|| format!("could not download version {}", release.version))?;
        let signature = self.get(&format!("{path}.minisig"), 4 << 10)
            .with_context(|| format!("could not download the signature of version {}", release.version))?;
        self.verify(&build, &String::from_utf8_lossy(&signature), release)?;
        log::write(format!("update: signature of version {} holds", release.version));
        Ok(build)
    }

    fn get(&self, path: &str, limit: u64) -> Result<Vec<u8>> {
        let url = format!("{}/{path}", self.releases);
        log::write(format!("update: GET {url}"));
        let mut response = self.agent.get(&url).call()?;
        let body = response.body_mut().with_config().limit(limit).read_to_vec()?;
        log::write(format!("update: {} bytes from {url}", body.len()));
        Ok(body)
    }

    fn verify(&self, build: &[u8], signature: &str, release: &Release) -> Result<()> {
        let refuse = |reason: String| anyhow!("refusing the download of version {}: {reason}", release.version);
        let signature = Signature::decode(signature).map_err(|error| refuse(format!("unreadable signature ({error})")))?;
        self.key.verify(build, &signature, false)
            .map_err(|error| refuse(format!("it is not signed with the trashpanda release key ({error})")))?;
        // The trusted comment is signed, so it can be believed from here on.
        let comment = signature.trusted_comment();
        let (version, platform) = match comment.split(' ').collect::<Vec<_>>()[..] {
            ["trashpanda", version, platform] => (version.parse::<Version>().ok(), platform),
            _ => (None, ""),
        };
        let Some(version) = version else { return Err(refuse(format!("unexpected signed comment {comment:?}"))) };
        if platform != self.platform {
            return Err(refuse(format!("it is signed as a build for {platform}, not {}", self.platform)));
        }
        if version != release.version {
            return Err(refuse(format!("it is signed as version {version}")));
        }
        if version <= self.current {
            return Err(refuse(format!("it is not newer than this version, {}", self.current)));
        }
        Ok(())
    }

    /// Download and check the release, and swap it in for the running
    /// program. Returns the program's path, to start the new version from.
    pub fn install(&self, release: &Release) -> Result<PathBuf> {
        let exe = std::env::current_exe().context("could not find the running program")?;
        log::write(format!("update: installing version {} over {exe:?}", release.version));
        #[cfg(windows)]
        self.install_at(release, &exe, |staged| swap_on_windows(&exe, staged))?;
        #[cfg(not(windows))]
        self.install_at(release, &exe, |staged| self_replace::self_replace(staged))?;
        Ok(exe)
    }

    /// `install` for the program at `exe`, with `swap` putting the checked
    /// build in its place. The build is written into the program's folder,
    /// so that renaming it into place stays on one drive.
    pub fn install_at(&self, release: &Release, exe: &Path, swap: impl FnOnce(&Path) -> io::Result<()>) -> Result<()> {
        let dir = exe.parent().context("the program has no folder")?;
        let unwritable = |error| anyhow!(Unwritable { dir: dir.into(), page: release.page.clone(), error });
        // Find out whether the folder is writable before downloading anything.
        let (staged, mut file) = create_staged(dir).map_err(unwritable)?;
        log::write(format!("update: writing the new build to {staged:?}"));
        let result = (|| {
            let build = self.download(release)?;
            file.write_all(&build).and_then(|()| file.sync_all()).map_err(unwritable)?;
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).map_err(unwritable)?;
            }
            swap(&staged).map_err(unwritable)
        })();
        _ = fs::remove_file(&staged);
        log::write(format!("update: install {}", if result.is_ok() { "done" } else { "failed" }));
        result
    }
}

/// Create a new, empty file in `dir` to write a build to. Each attempt gets
/// a file of its own, so that a second update running at the same time
/// cannot empty or delete a build the first has already checked.
fn create_staged(dir: &Path) -> io::Result<(PathBuf, File)> {
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    loop {
        let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{STAGED}-{}-{attempt}", std::process::id()));
        match File::options().write(true).create_new(true).open(&path) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return result.map(|file| (path, file)),
        }
    }
}

/// Put the build at `staged` in place of the running program at `exe`.
/// `self_replace` moves the program aside before copying the build in, so
/// a failed copy (a full drive, say) leaves no program at all. Renaming
/// within the folder needs no space, and the first rename is undone if the
/// second fails. The old program is deleted once this process exits.
#[cfg(windows)]
fn swap_on_windows(exe: &Path, staged: &Path) -> io::Result<()> {
    let mut old = staged.as_os_str().to_owned();
    old.push(".old");
    let old = PathBuf::from(old);
    fs::rename(exe, &old)?;
    if let Err(error) = fs::rename(staged, exe) {
        _ = fs::rename(&old, exe);
        return Err(error);
    }
    // Should this fail, the old program is left behind as a stray file.
    _ = self_replace::self_delete_at(&old);
    Ok(())
}

/// Start the program at `exe` again, which opens its window; this process
/// is left to exit.
pub fn restart(exe: &Path) -> io::Result<()> {
    let mut command = Command::new(exe);
    #[cfg(windows)]
    {
        // The window needs no console of its own.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(drop)
}
