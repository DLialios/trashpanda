//! The download engine. Every chunk is written straight into its file at its
//! offset, so resuming needs no state: a run first checks which chunks are
//! already on disk with the right SHA-1, then fetches the rest.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail, ensure};
use sha1::{Digest, Sha1};

use crate::depot::{self, Chunk, Depot, Entry, Package};

const SERVER_LIST: &str = "https://api.steampowered.com/IContentServerDirectoryService/\
                           GetServersForSteamPipe/v1/?cell_id=0&max_servers=20";
/// After this many chunks fail (each after retries on every server), give up.
const MAX_FAILURES: usize = 20;
/// Pause before the second round of retries; it doubles for the third.
const RETRY_DELAY: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 4000 });

/// Progress shared between the download threads and the display.
#[derive(Default)]
pub struct Progress {
    /// Bytes of file content in the package.
    pub total: AtomicU64,
    /// Bytes of file content finished, whether found on disk or downloaded.
    pub done: AtomicU64,
    /// The part of `done` that had to be downloaded.
    pub fetched: AtomicU64,
    /// Bytes received from the network (chunks are compressed, so this
    /// usually differs from `fetched`).
    pub received: AtomicU64,
    /// Set to ask the download to stop after the chunks in flight.
    pub stop: AtomicBool,
    log: Mutex<Vec<String>>,
}

impl Progress {
    pub fn log(&self, line: impl Into<String>) {
        self.log.lock().unwrap().push(line.into());
    }

    /// Log lines from index `from` onwards.
    pub fn lines(&self, from: usize) -> Vec<String> {
        self.log.lock().unwrap().get(from..).unwrap_or_default().to_vec()
    }

    pub fn fraction(&self) -> f32 {
        let total = self.total.load(Relaxed);
        if total == 0 { 0.0 } else { self.done.load(Relaxed) as f32 / total as f32 }
    }
}

#[derive(Debug, Default)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
    pub fetched: usize,
    pub reused: usize,
    /// `Progress::stop` ended the run early; running again resumes it.
    pub paused: bool,
    /// What is on disk now, unless the run stopped before it finished checking.
    pub on_disk: Option<OnDisk>,
}

/// Download all depots of `package` into `output`. `previous` is an earlier
/// check of the same folder, which spares reading unchanged files again.
pub fn run(package: &Package, output: &Path, jobs: usize, previous: Option<&OnDisk>, progress: &Progress)
           -> Result<Summary> {
    for line in describe(package) {
        progress.log(line);
    }
    for warning in &package.warnings {
        progress.log(warning);
    }
    let agent = agent(jobs);
    let servers = content_servers(&agent)?;
    let hosts: Vec<&str> = servers.iter().map(|url| url.trim_start_matches("https://")).collect();
    progress.log(format!("Servers (Steam picks them by your location): {}", hosts.join(", ")));
    download(package, output, jobs, &agent, &servers, previous, progress)
}

/// A line for the app, if the package names it, and one per depot.
pub fn describe(package: &Package) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(app) = package.app {
        let name = package.app_name.as_ref().map(|name| format!(": {name}")).unwrap_or_default();
        let build = package.build.as_ref().map(|build| format!(", build {build}")).unwrap_or_default();
        lines.push(format!("App {app}{name}{build}"));
    }
    for depot in &package.depots {
        let created = depot.created.map(|time| format!(", {}", date(time))).unwrap_or_default();
        let name = depot.name.as_ref().map(|name| format!(" - {name}")).unwrap_or_default();
        lines.push(format!("Depot {} (manifest {}{created}): {} files, {}{name}",
                           depot.id, depot.manifest, depot.files(), human(depot.size())));
    }
    lines
}

pub fn agent(jobs: usize) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent(concat!("rin-dl/", env!("CARGO_PKG_VERSION")))
        .max_idle_connections(jobs * 4)
        .max_idle_connections_per_host(jobs)
        .build()
        .into()
}

/// Ask Steam's public directory for content servers; no login is needed.
pub fn content_servers(agent: &ureq::Agent) -> Result<Vec<String>> {
    let body = agent.get(SERVER_LIST).call().context("fetching the Steam content server list")?
        .body_mut().read_to_string()?;
    let servers = parse_servers(&body)?;
    ensure!(!servers.is_empty(), "Steam returned no unrestricted HTTPS content servers");
    Ok(servers)
}

pub fn parse_servers(json: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let mut servers = Vec::new();
    for server in value["response"]["servers"].as_array().context("unexpected server list")? {
        let restricted = server["allowed_app_ids"].as_array().is_some_and(|ids| !ids.is_empty());
        let usable = matches!(server["type"].as_str(), Some("SteamCache" | "CDN"))
            && matches!(server["https_support"].as_str(), Some("mandatory" | "optional"))
            && !restricted
            && server["use_as_proxy"].as_bool() != Some(true);
        let host = server["vhost"].as_str().filter(|h| !h.is_empty()).or(server["host"].as_str());
        if let (true, Some(host)) = (usable, host) {
            let url = format!("https://{host}");
            if !servers.contains(&url) {
                servers.push(url);
            }
        }
    }
    servers.truncate(8);
    Ok(servers)
}

/// One output file: where it goes and what it holds.
struct Target<'a> {
    path: PathBuf,
    entry: &'a Entry,
    depot: &'a Depot,
}

impl Target<'_> {
    fn source(&self) -> (u32, u64) {
        (self.depot.id, self.depot.manifest)
    }
}

/// Where every entry of `package` goes: the folders, then the files. Refuses
/// unsafe paths and two files at one path, and touches nothing on disk.
fn plan<'a>(package: &'a Package, output: &Path) -> Result<(Vec<PathBuf>, Vec<Target<'a>>)> {
    let (mut folders, mut files, mut seen) = (Vec::new(), Vec::new(), HashMap::new());
    for depot in &package.depots {
        for entry in &depot.entries {
            let path = destination(output, &entry.name)?;
            if entry.is_dir() {
                folders.push(path);
                continue;
            }
            // Depots may share folders, but two files at one path would corrupt each other.
            let unique = if cfg!(windows) { entry.name.to_lowercase() } else { entry.name.clone() };
            match seen.insert(unique, depot.id) {
                Some(other) if other == depot.id => bail!("{:?} appears twice in depot {other}", entry.name),
                Some(other) => bail!("{:?} is in both depot {other} and depot {}; download only one of them \
                                      (they may be the same game for different systems)", entry.name, depot.id),
                None => {}
            }
            files.push(Target { path, entry, depot });
        }
    }
    Ok((folders, files))
}

/// What is already in an output folder: for each file found, its length and
/// modification time when it was checked, and which of its chunks matched
/// the manifest. A later check trusts this for files that have not changed.
#[derive(Debug, Default)]
pub struct OnDisk {
    files: HashMap<PathBuf, Found>,
}

#[derive(Clone, Debug)]
struct Found {
    /// The depot and manifest the file was checked against.
    source: (u32, u64),
    len: u64,
    modified: Option<SystemTime>,
    good: Vec<bool>,
}

impl OnDisk {
    fn is_good(&self, path: &Path, chunk: usize) -> bool {
        self.files.get(path).is_some_and(|found| found.good[chunk])
    }

    /// Mark a chunk as written and correct.
    fn add(&mut self, target: &Target, chunk: usize) {
        let blank = || Found { source: target.source(), len: 0, modified: None,
                               good: vec![false; target.entry.chunks.len()] };
        self.files.entry(target.path.clone()).or_insert_with(blank).good[chunk] = true;
    }

    /// Record a file's length and modification time as they are now.
    fn restat(&mut self, path: &Path) -> io::Result<()> {
        if let Some(found) = self.files.get_mut(path) {
            let meta = fs::metadata(path)?;
            (found.len, found.modified) = (meta.len(), meta.modified().ok());
        }
        Ok(())
    }
}

/// Find which chunks of `package` are already in `output` and add their size
/// to `progress.done`, without writing anything. Files unchanged since
/// `previous` was made are not read again. `None` means `progress.stop` cut
/// the check short.
pub fn check(package: &Package, output: &Path, previous: Option<&OnDisk>, progress: &Progress)
             -> Result<Option<OnDisk>> {
    let (_, targets) = plan(package, output)?;
    progress.total.store(targets.iter().map(|target| target.entry.size).sum(), Relaxed);
    check_files(&targets, previous, progress)
}

fn check_files(targets: &[Target], previous: Option<&OnDisk>, progress: &Progress) -> Result<Option<OnDisk>> {
    let mut on_disk = OnDisk::default();
    let mut queue = Vec::new();
    for (t, target) in targets.iter().enumerate() {
        let meta = match fs::metadata(&target.path) {
            Ok(meta) if meta.is_file() => meta,
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("reading {}", target.path.display()));
            }
            _ => continue, // Not downloaded yet.
        };
        let chunks = &target.entry.chunks;
        let found = Found { source: target.source(), len: meta.len(), modified: meta.modified().ok(),
                            good: vec![false; chunks.len()] };
        let unchanged = previous.and_then(|previous| previous.files.get(&target.path)).filter(|old| {
            (old.source, old.len, old.modified) == (found.source, found.len, found.modified)
                && found.modified.is_some()
        });
        if let Some(old) = unchanged {
            let size: u64 = chunks.iter().zip(&old.good).filter(|(_, good)| **good).map(|(c, _)| u64::from(c.size)).sum();
            progress.done.fetch_add(size, Relaxed);
            on_disk.files.insert(target.path.clone(), old.clone());
            continue;
        }
        let present = |c: &usize| chunks[*c].offset + u64::from(chunks[*c].size) <= found.len;
        queue.extend((0..chunks.len()).filter(present).map(|c| (t, c)));
        on_disk.files.insert(target.path.clone(), found);
    }

    // Disks are slow with many readers at once, so use a few threads.
    let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let next = AtomicUsize::new(0);
    let matched = thread::scope(|scope| {
        let workers: Vec<_> = (0..threads).map(|_| scope.spawn(|| -> Result<Vec<(usize, usize)>> {
            let (mut matched, mut buffer) = (Vec::new(), Vec::new());
            while !progress.stop.load(Relaxed) {
                let Some(&(t, c)) = queue.get(next.fetch_add(1, Relaxed)) else { break };
                let (path, chunk) = (&targets[t].path, &targets[t].entry.chunks[c]);
                buffer.resize(chunk.size as usize, 0);
                File::open(path).and_then(|file| read_at(&file, &mut buffer, chunk.offset))
                    .with_context(|| format!("reading {}", path.display()))?;
                if Sha1::digest(&buffer)[..] == chunk.sha {
                    progress.done.fetch_add(chunk.size.into(), Relaxed);
                    matched.push((t, c));
                }
            }
            Ok(matched)
        })).collect();
        workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Result<Vec<_>>>()
    })?;
    if progress.stop.load(Relaxed) {
        return Ok(None);
    }
    for (t, c) in matched.into_iter().flatten() {
        on_disk.add(&targets[t], c);
    }
    Ok(Some(on_disk))
}

/// The open handle of a file being written, shared by the workers writing it
/// and closed after its last queued chunk.
#[derive(Default)]
struct Slot {
    file: Mutex<Option<Arc<File>>>,
    remaining: AtomicUsize,
}

impl Slot {
    fn write(&self, path: &Path, data: &[u8], offset: u64) -> io::Result<()> {
        let mut slot = self.file.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Arc::new(OpenOptions::new().write(true).create(true).truncate(false).open(path)?));
        }
        let file = slot.clone().unwrap();
        drop(slot);
        write_at(&file, data, offset)
    }

    fn finish_chunk(&self) {
        if self.remaining.fetch_sub(1, Relaxed) == 1 {
            self.file.lock().unwrap().take();
        }
    }
}

struct Work<'a> {
    targets: &'a [Target<'a>],
    slots: Vec<Slot>,
    /// (target, chunk) pairs still to fetch, in file and offset order.
    queue: Vec<(usize, usize)>,
    next: AtomicUsize,
    failed: AtomicUsize,
    fatal: Mutex<Option<anyhow::Error>>,
    agent: &'a ureq::Agent,
    servers: &'a [String],
    progress: &'a Progress,
}

/// Download every file of every depot in `package` that is not already in
/// `output`. `previous` is an earlier check of the same folder.
pub fn download(package: &Package, output: &Path, jobs: usize, agent: &ureq::Agent, servers: &[String],
                previous: Option<&OnDisk>, progress: &Progress) -> Result<Summary> {
    ensure!(!servers.is_empty(), "no content servers");
    let (folders, targets) = plan(package, output)?;
    let parents = targets.iter().filter_map(|target| target.path.parent());
    for folder in folders.iter().map(PathBuf::as_path).chain(parents) {
        fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    }
    let mut summary = Summary {
        files: targets.len(),
        bytes: targets.iter().map(|target| target.entry.size).sum(),
        ..Summary::default()
    };
    progress.total.store(summary.bytes, Relaxed);
    progress.log(format!("Output: {}", output.display()));
    let started = Instant::now();

    let Some(mut on_disk) = check_files(&targets, previous, progress)? else {
        progress.log("Paused while checking what is already downloaded.");
        summary.paused = true;
        return Ok(summary);
    };
    let found = progress.done.load(Relaxed);
    if found > 0 {
        progress.log(format!("Already downloaded: {} of {}.", human(found), human(summary.bytes)));
    }
    let queue: Vec<(usize, usize)> = targets.iter().enumerate()
        .flat_map(|(t, target)| (0..target.entry.chunks.len()).map(move |c| (t, c)))
        .filter(|&(t, c)| !on_disk.is_good(&targets[t].path, c))
        .collect();
    summary.reused = targets.iter().map(|target| target.entry.chunks.len()).sum::<usize>() - queue.len();
    let mut slots: Vec<Slot> = targets.iter().map(|_| Slot::default()).collect();
    for &(t, _) in &queue {
        *slots[t].remaining.get_mut() += 1;
    }
    let work = Work {
        targets: &targets, slots, queue, next: AtomicUsize::new(0), failed: AtomicUsize::new(0),
        fatal: Mutex::new(None), agent, servers, progress,
    };
    let fetching = Instant::now();
    let written: Vec<(usize, usize)> = thread::scope(|scope| {
        let work = &work;
        let workers: Vec<_> = (0..jobs.max(1)).map(|id| scope.spawn(move || work.worker(id))).collect();
        workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect()
    });
    let fetch_time = fetching.elapsed();
    drop(work.slots); // Close the files of chunks left unfinished.

    if let Some(error) = work.fatal.into_inner().unwrap() {
        return Err(error);
    }
    let failed = work.failed.into_inner();
    ensure!(failed == 0, "{failed} chunks could not be downloaded. Run again to retry them.");
    summary.fetched = written.len();
    let mut touched = HashSet::new();
    for (t, c) in written {
        on_disk.add(&targets[t], c);
        touched.insert(t);
    }
    let paused = progress.stop.load(Relaxed);
    if !paused {
        for (t, target) in targets.iter().enumerate() {
            if tidy(target).with_context(|| format!("finishing {}", target.path.display()))? {
                touched.insert(t);
            }
        }
    }
    for t in touched {
        on_disk.restat(&targets[t].path).with_context(|| format!("reading {}", targets[t].path.display()))?;
    }
    summary.on_disk = Some(on_disk);
    if paused {
        progress.log("Paused. What is on disk is kept; resuming downloads the rest.");
        summary.paused = true;
        return Ok(summary);
    }

    let complete = format!("Complete: {} files, {} in {}.", summary.files, human(summary.bytes),
                           duration(started.elapsed().as_secs()));
    let chunks = summary.fetched + summary.reused;
    if summary.fetched == 0 {
        progress.log(format!("{complete} Nothing to download; all {chunks} chunks were already on disk."));
    } else {
        let received = progress.received.load(Relaxed);
        let average = received as f64 / fetch_time.as_secs_f64().max(0.001);
        progress.log(format!("{complete} Downloaded {} at {}/s on average; {} of {chunks} chunks were already on disk.",
                             human(received), human(average as u64), summary.reused));
    }
    Ok(summary)
}

/// Give a downloaded file its exact size and its permissions. Returns
/// whether it had to be cut short.
fn tidy(target: &Target) -> io::Result<bool> {
    let file = OpenOptions::new().write(true).create(true).truncate(false).open(&target.path)?;
    let cut = file.metadata()?.len() > target.entry.size;
    if cut {
        file.set_len(target.entry.size)?;
    }
    set_mode(&file, target.entry)?;
    Ok(cut)
}

impl Work<'_> {
    /// Fetch queued chunks until none are left or the run stops. Returns the
    /// (target, chunk) pairs it wrote.
    fn worker(&self, id: usize) -> Vec<(usize, usize)> {
        let mut server = id; // Spread the workers over the servers.
        let mut written = Vec::new();
        while !self.progress.stop.load(Relaxed) {
            let Some(&(t, c)) = self.queue.get(self.next.fetch_add(1, Relaxed)) else { break };
            let (target, chunk) = (&self.targets[t], &self.targets[t].entry.chunks[c]);
            match self.fetch(target.depot, chunk, &mut server) {
                Ok(plain) => match self.slots[t].write(&target.path, &plain, chunk.offset) {
                    Ok(()) => {
                        self.progress.done.fetch_add(chunk.size.into(), Relaxed);
                        self.progress.fetched.fetch_add(chunk.size.into(), Relaxed);
                        written.push((t, c));
                    }
                    Err(error) => self.abort(anyhow!(error).context(target.path.display().to_string())),
                },
                Err(_) if self.progress.stop.load(Relaxed) => {}
                Err(error) => {
                    let failed = self.failed.fetch_add(1, Relaxed) + 1;
                    self.progress.log(format!("Chunk of {:?} failed: {error:#}", target.entry.name));
                    if failed >= MAX_FAILURES {
                        self.abort(anyhow!("giving up after {failed} failed chunks; last error: {error:#}"));
                    }
                }
            }
            self.slots[t].finish_chunk();
        }
        written
    }

    fn abort(&self, error: anyhow::Error) {
        self.fatal.lock().unwrap().get_or_insert(error);
        self.progress.stop.store(true, Relaxed);
    }

    /// Try each server in turn, three rounds, backing off between rounds.
    fn fetch(&self, depot: &Depot, chunk: &Chunk, server: &mut usize) -> Result<Vec<u8>> {
        let sha: String = chunk.sha.iter().map(|b| format!("{b:02x}")).collect();
        let mut error = anyhow!("no servers");
        for round in 0..3 {
            if round > 0 {
                thread::sleep(RETRY_DELAY * round);
            }
            for _ in 0..self.servers.len() {
                if self.progress.stop.load(Relaxed) {
                    bail!("stopped");
                }
                let url = format!("{}/depot/{}/chunk/{sha}", self.servers[*server % self.servers.len()], depot.id);
                let result = self.agent.get(&url).call()
                    .and_then(|mut response| response.body_mut().with_config().limit(64 << 20).read_to_vec())
                    .inspect(|data| _ = self.progress.received.fetch_add(data.len() as u64, Relaxed))
                    .map_err(anyhow::Error::from)
                    .and_then(|data| depot::decode_chunk(&data, &depot.key, chunk));
                match result {
                    Ok(plain) => return Ok(plain),
                    Err(e) => {
                        error = e;
                        *server += 1;
                    }
                }
            }
        }
        Err(error)
    }
}

/// The output path for a manifest entry, refusing paths through symlinks.
fn destination(root: &Path, name: &str) -> Result<PathBuf> {
    depot::check_name(name)?;
    let mut path = root.to_path_buf();
    for part in name.split('/') {
        path.push(part);
        let is_link = path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
        ensure!(!is_link, "output path contains a symbolic link: {}", path.display());
    }
    Ok(path)
}

#[cfg(unix)]
fn set_mode(file: &File, entry: &Entry) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(if entry.is_executable() { 0o755 } else { 0o644 }))
}

#[cfg(not(unix))]
fn set_mode(_: &File, _: &Entry) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        match file.seek_read(buffer, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => (buffer, offset) = (&mut buffer[n..], offset + n as u64),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut data: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        match file.seek_write(data, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => (data, offset) = (&data[n..], offset + n as u64),
        }
    }
    Ok(())
}

pub fn human(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 4 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", ["", "KiB", "MiB", "GiB", "TiB"][unit]) }
}

pub fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// `YYYY-MM-DD` (UTC) for a Unix time.
pub fn date(unix: u64) -> String {
    // Howard Hinnant's civil_from_days, with days counted from 0000-03-01.
    let days = unix / 86_400 + 719_468;
    let (era, day_of_era) = (days / 146_097, days % 146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    format!("{year}-{month:02}-{day:02}")
}

/// How far back the speed and time-left estimates look.
const WINDOW: Duration = Duration::from_secs(10);

/// Download speed and time left, averaged over the last few seconds.
#[derive(Default)]
pub struct Meter {
    /// (time, bytes received, bytes fetched), oldest first.
    samples: VecDeque<(Instant, u64, u64)>,
}

#[derive(Debug, PartialEq)]
pub struct Rates {
    /// Network bytes per second; `None` for the first second.
    pub speed: Option<f64>,
    /// Seconds until done; `None` while nothing is being downloaded.
    pub left: Option<f64>,
}

impl Meter {
    /// Record the counters at `now` and estimate from the last few seconds.
    /// Time left is the content still to go at the rate content is being
    /// downloaded, so chunks found on disk do not distort it.
    pub fn update(&mut self, progress: &Progress, now: Instant) -> Rates {
        let (received, fetched) = (progress.received.load(Relaxed), progress.fetched.load(Relaxed));
        self.samples.push_back((now, received, fetched));
        while self.samples.len() > 2 && now.duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
        let (then, received_then, fetched_then) = self.samples[0];
        let seconds = now.duration_since(then).as_secs_f64();
        if seconds < 1.0 {
            return Rates { speed: None, left: None };
        }
        let fetch_rate = fetched.saturating_sub(fetched_then) as f64 / seconds;
        let remaining = progress.total.load(Relaxed).saturating_sub(progress.done.load(Relaxed));
        Rates {
            speed: Some(received.saturating_sub(received_then) as f64 / seconds),
            left: match remaining {
                0 => Some(0.0),
                _ if fetch_rate > 0.0 => Some(remaining as f64 / fetch_rate),
                _ => None,
            },
        }
    }
}

impl Rates {
    pub fn speed_text(&self) -> String {
        self.speed.map_or("--".into(), |speed| format!("{}/s", human(speed as u64)))
    }

    pub fn left_text(&self) -> String {
        self.left.map_or("--".into(), |seconds| duration(seconds.round() as u64))
    }
}
