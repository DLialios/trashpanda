//! The download engine. Every chunk is written straight into its file at its
//! offset, so resuming needs no state: a run first checks which chunks are
//! already on disk with the right SHA-1, then copies into place the chunks
//! that an older version has somewhere else, then fetches the rest.

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
use crate::moved::{self, Move, Region, Step};
use crate::{installed, log};

const SERVER_LIST: &str = "https://api.steampowered.com/IContentServerDirectoryService/\
                           GetServersForSteamPipe/v1/?cell_id=0&max_servers=20";
/// After this many chunks fail (each after retries on every server), give up.
const MAX_FAILURES: usize = 20;
/// Pause before the second round of retries; it doubles for the third.
const RETRY_DELAY: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 4000 });
/// A server that takes longer than this to connect, or then goes this long
/// without sending a byte, is given up on and the next one tried. Without
/// it a hung connection held up the run, and a pause, for two minutes.
const STALL: Duration = Duration::from_millis(if cfg!(test) { 1000 } else { 10_000 });
/// How often a download logs how far it has got.
const REPORT_EVERY: Duration = Duration::from_secs(15);
/// The most content kept in memory while chunks that swap places are copied.
const HOLD: u64 = 256 << 20;

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
    /// Bytes of content that are on disk but not where they belong, as an
    /// older version had them elsewhere: these are copied, not downloaded.
    pub elsewhere: AtomicU64,
    /// Set while a download looks for such content and copies it into place,
    /// which uses the disk and not the network.
    pub local: AtomicBool,
    /// Set to ask the download to stop after the chunks in flight.
    pub stop: AtomicBool,
    log: Mutex<Vec<String>>,
}

impl Progress {
    /// Add a line for the user, which also goes into the session log.
    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        log::write(&line);
        self.log.lock().unwrap().push(line);
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
        .timeout_connect(Some(STALL))
        .timeout_per_read(Some(STALL))
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent(concat!("trashpanda/", env!("CARGO_PKG_VERSION")))
        .max_idle_connections(jobs * 4)
        .max_idle_connections_per_host(jobs)
        .build()
        .into()
}

/// Ask Steam's public directory for content servers; no login is needed.
pub fn content_servers(agent: &ureq::Agent) -> Result<Vec<String>> {
    let failed = "could not get the list of download servers from Steam";
    let started = Instant::now();
    log::write(format!("servers: GET {SERVER_LIST}"));
    let mut response = agent.get(SERVER_LIST).call().context(failed)?;
    let body = response.body_mut().read_to_string().context(failed)?;
    log::write(format!("servers: {} after {} ms: {}", response.status(), started.elapsed().as_millis(),
                       log::clip(&body, 16 << 10)));
    let servers = parse_servers(&body).context(failed)?;
    log::write(format!("servers: usable {servers:?}"));
    ensure!(!servers.is_empty(), "Steam offered no usable download servers");
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
                Some(other) if other == depot.id => bail!("depot {other} lists {:?} twice", entry.name),
                Some(other) => bail!("depots {other} and {} both contain {:?}; choose only one of them",
                                     depot.id, entry.name),
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
    /// Chunks seen somewhere other than their place: the file and the offset.
    /// Each is checked again when it is copied, so an entry may be out of date.
    elsewhere: HashMap<[u8; 20], (PathBuf, u64)>,
    /// The files already searched for such chunks.
    searched: HashSet<PathBuf>,
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
/// to `progress.done`, and the size of those that are there but out of place
/// to `progress.elsewhere`, without writing anything. Files unchanged since
/// `previous` was made are not read again. `None` means `progress.stop` cut
/// the check short.
pub fn check(package: &Package, output: &Path, previous: Option<&OnDisk>, progress: &Progress)
             -> Result<Option<OnDisk>> {
    let (_, targets) = plan(package, output)?;
    progress.total.store(targets.iter().map(|target| target.entry.size).sum(), Relaxed);
    log::write(format!("check: {output:?}, depots {:?}, {} bytes",
                       package.depots.iter().map(|depot| depot.id).collect::<Vec<_>>(), progress.total.load(Relaxed)));
    let Some(mut on_disk) = check_files(&targets, previous, progress)? else { return Ok(None) };
    let complete = find_elsewhere(&targets, &installed::leftovers(output, package), &mut on_disk, progress);
    Ok(complete.then_some(on_disk))
}

fn check_files(targets: &[Target], previous: Option<&OnDisk>, progress: &Progress) -> Result<Option<OnDisk>> {
    let started = Instant::now();
    let mut on_disk = OnDisk::default();
    if let Some(previous) = previous {
        (on_disk.elsewhere, on_disk.searched) = (previous.elsewhere.clone(), previous.searched.clone());
    }
    let mut queue = Vec::new();
    let (mut present, mut reused) = (0, 0);
    for (t, target) in targets.iter().enumerate() {
        let meta = match fs::metadata(&target.path) {
            Ok(meta) if meta.is_file() => meta,
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("could not read {}", target.path.display()));
            }
            _ => continue, // Not downloaded yet.
        };
        present += 1;
        let chunks = &target.entry.chunks;
        let found = Found { source: target.source(), len: meta.len(), modified: meta.modified().ok(),
                            good: vec![false; chunks.len()] };
        let unchanged = previous.and_then(|previous| previous.files.get(&target.path)).filter(|old| {
            (old.source, old.len, old.modified) == (found.source, found.len, found.modified)
                && found.modified.is_some()
        });
        if let Some(old) = unchanged {
            reused += 1;
            let size: u64 = chunks.iter().zip(&old.good).filter(|(_, good)| **good).map(|(c, _)| u64::from(c.size)).sum();
            progress.done.fetch_add(size, Relaxed);
            on_disk.files.insert(target.path.clone(), old.clone());
            continue;
        }
        let present = |c: &usize| chunks[*c].offset + u64::from(chunks[*c].size) <= found.len;
        queue.extend((0..chunks.len()).filter(present).map(|c| (t, c)));
        on_disk.searched.remove(&target.path); // It changed, so it may hold other content now.
        on_disk.files.insert(target.path.clone(), found);
    }

    // Disks are slow with many readers at once, so use a few threads.
    let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    log::write(format!("check: {} files, {present} on disk, {reused} of those unchanged since the last check \
                        ({} files known from it); reading {} chunks with {threads} threads", targets.len(),
                       previous.map_or(0, |previous| previous.files.len()), queue.len()));
    let next = AtomicUsize::new(0);
    let matched = thread::scope(|scope| {
        let workers: Vec<_> = (0..threads).map(|_| scope.spawn(|| -> Result<Vec<(usize, usize)>> {
            let (mut matched, mut buffer) = (Vec::new(), Vec::new());
            while !progress.stop.load(Relaxed) {
                let Some(&(t, c)) = queue.get(next.fetch_add(1, Relaxed)) else { break };
                let (path, chunk) = (&targets[t].path, &targets[t].entry.chunks[c]);
                buffer.resize(chunk.size as usize, 0);
                File::open(path).and_then(|file| read_at(&file, &mut buffer, chunk.offset))
                    .with_context(|| format!("could not read {}", path.display()))?;
                if Sha1::digest(&buffer)[..] == chunk.sha {
                    progress.done.fetch_add(chunk.size.into(), Relaxed);
                    matched.push((t, c));
                }
            }
            Ok(matched)
        })).collect();
        workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Result<Vec<_>>>()
    })?;
    let seconds = started.elapsed().as_secs_f64();
    if progress.stop.load(Relaxed) {
        log::write(format!("check: stopped after {seconds:.3}s"));
        return Ok(None);
    }
    log::write(format!("check: {} of {} chunks read match; {} bytes already there; {seconds:.3}s",
                       matched.iter().map(Vec::len).sum::<usize>(), queue.len(), progress.done.load(Relaxed)));
    for (t, c) in matched.into_iter().flatten() {
        on_disk.add(&targets[t], c);
    }
    Ok(Some(on_disk))
}

/// Find the chunks that are not in place but are somewhere else on disk: in
/// what an older version left in the files of `targets`, or in `stale`, the
/// files of that version which the new one no longer has. Records where in
/// `on_disk` and their size in `progress.elsewhere`, and writes nothing.
/// Returns false if `progress.stop` cut it short.
fn find_elsewhere(targets: &[Target], stale: &[PathBuf], on_disk: &mut OnDisk, progress: &Progress) -> bool {
    let started = Instant::now();
    let mut missing: HashMap<[u8; 20], &Chunk> = HashMap::new();
    for target in targets {
        for (c, chunk) in target.entry.chunks.iter().enumerate() {
            if !on_disk.is_good(&target.path, c) {
                missing.insert(chunk.sha, chunk);
            }
        }
    }
    on_disk.elsewhere.retain(|sha, _| missing.contains_key(sha));
    if !missing.is_empty() {
        // A chunk the package has twice may be in place at one of the two.
        for target in targets {
            for (c, chunk) in target.entry.chunks.iter().enumerate() {
                if missing.contains_key(&chunk.sha) && on_disk.is_good(&target.path, c) {
                    on_disk.elsewhere.insert(chunk.sha, (target.path.clone(), chunk.offset));
                }
            }
        }
    }
    let wanted: Vec<([u8; 20], u32)> = missing.values()
        .filter(|chunk| chunk.size as usize == moved::WINDOW && !on_disk.elsewhere.contains_key(&chunk.sha))
        .map(|chunk| (chunk.sha, chunk.sum))
        .collect();

    // Where an older version's content may be: wherever a file does not hold
    // the new version's, past the new version's end, and in the stale files.
    let mut files: Vec<PathBuf> = targets.iter().map(|target| target.path.clone()).collect();
    let mut regions = Vec::new();
    let mut add = |file: usize, start: u64, end: u64| {
        if end.saturating_sub(start) >= moved::WINDOW as u64 {
            regions.push(Region { file, start, end });
        }
    };
    for (t, target) in targets.iter().enumerate() {
        let Some(found) = on_disk.files.get(&target.path).filter(|_| !on_disk.searched.contains(&target.path)) else {
            continue;
        };
        let mut run = None; // Where the chunks not in place began.
        for (chunk, good) in target.entry.chunks.iter().zip(&found.good) {
            match (*good, run) {
                (false, None) => run = Some(chunk.offset),
                (true, Some(start)) => {
                    add(t, start, chunk.offset.min(found.len));
                    run = None;
                }
                _ => {}
            }
        }
        add(t, run.unwrap_or(target.entry.size), found.len);
    }
    for path in stale.iter().filter(|path| !on_disk.searched.contains(*path)) {
        if let Ok(meta) = fs::metadata(path) {
            add(files.len(), 0, meta.len());
            files.push(path.clone());
        }
    }
    let size: u64 = regions.iter().map(|region| region.end - region.start).sum();
    log::write(format!("search: {} chunks missing, {} of them known elsewhere, {} to look for in {size} bytes of {} files",
                       missing.len(), on_disk.elsewhere.len(), wanted.len(), regions.len()));
    if !wanted.is_empty() && !regions.is_empty() {
        progress.log(format!("Looking through {} that an older version left for content this one still has.", human(size)));
        let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let places = moved::search(&files, &regions, &wanted, threads, &progress.stop);
        log::write(format!("search: found {} of {} chunks with {threads} threads in {:.3}s",
                           places.len(), wanted.len(), started.elapsed().as_secs_f64()));
        on_disk.elsewhere.extend(places.into_iter().map(|(sha, (file, offset))| (sha, (files[file].clone(), offset))));
    }
    if progress.stop.load(Relaxed) {
        return false;
    }
    on_disk.searched.extend(files.into_iter().chain(stale.iter().cloned()));
    let mut elsewhere = 0;
    for target in targets {
        for (c, chunk) in target.entry.chunks.iter().enumerate() {
            if !on_disk.is_good(&target.path, c) && on_disk.elsewhere.contains_key(&chunk.sha) {
                elsewhere += u64::from(chunk.size);
            }
        }
    }
    progress.elsewhere.store(elsewhere, Relaxed);
    true
}

/// Copy into place the chunks that `on_disk` has seen elsewhere, checking
/// each one's SHA-1 first. Returns the (target, chunk) pairs written; a chunk
/// that is no longer where it was seen is left to be downloaded.
fn relocate(targets: &[Target], on_disk: &OnDisk, progress: &Progress) -> Result<Vec<(usize, usize)>> {
    let started = Instant::now();
    let mut paths: Vec<&Path> = targets.iter().map(|target| target.path.as_path()).collect();
    let mut files: HashMap<&Path, usize> = paths.iter().enumerate().map(|(t, path)| (*path, t)).collect();
    // One move for each chunk, however many places it goes to.
    let mut by_sha: HashMap<[u8; 20], usize> = HashMap::new();
    let (mut moves, mut places): (Vec<Move>, Vec<Vec<(usize, usize)>>) = (Vec::new(), Vec::new());
    for (t, target) in targets.iter().enumerate() {
        for (c, chunk) in target.entry.chunks.iter().enumerate() {
            let Some((path, offset)) = on_disk.elsewhere.get(&chunk.sha).filter(|_| !on_disk.is_good(&target.path, c)) else {
                continue;
            };
            let index = *by_sha.entry(chunk.sha).or_insert_with(|| {
                let file = *files.entry(path.as_path()).or_insert_with(|| {
                    paths.push(path);
                    paths.len() - 1
                });
                moves.push(Move { from: (file, *offset), size: chunk.size, to: Vec::new() });
                places.push(Vec::new());
                moves.len() - 1
            });
            moves[index].to.push((t, chunk.offset));
            places[index].push((t, c));
        }
    }
    let steps = moved::schedule(&moves, HOLD);
    log::write(format!("move: {} chunks to copy, {} of them held in memory first, {} given up", moves.len(),
                       steps.iter().filter(|step| matches!(step, Step::Hold(_))).count(),
                       moves.len() - steps.iter().filter(|step| matches!(step, Step::Copy(_))).count()));
    let read = |index: usize| {
        let (from, chunk) = (moves[index].from, &targets[places[index][0].0].entry.chunks[places[index][0].1]);
        let mut data = vec![0; chunk.size as usize];
        let result = File::open(paths[from.0]).and_then(|file| read_at(&file, &mut data, from.1));
        if result.is_ok() && Sha1::digest(&data)[..] == chunk.sha {
            return Some(data);
        }
        log::write(format!("move: {:?} at {} no longer holds the chunk seen there ({result:?}); it will be downloaded",
                           paths[from.0], from.1));
        None
    };
    let (mut held, mut written) = (HashMap::new(), Vec::new());
    let mut open: Option<(usize, File)> = None;
    for step in steps {
        if progress.stop.load(Relaxed) {
            break;
        }
        let (Step::Hold(index) | Step::Copy(index)) = step;
        let Some(data) = held.remove(&index).or_else(|| read(index)) else { continue };
        if matches!(step, Step::Hold(_)) {
            held.insert(index, data);
            continue;
        }
        for &(t, c) in &places[index] {
            let path = &targets[t].path;
            if open.as_ref().is_none_or(|(file, _)| *file != t) {
                let file = OpenOptions::new().write(true).create(true).truncate(false).open(path);
                open = Some((t, file.with_context(|| format!("could not write {}", path.display()))?));
            }
            write_at(&open.as_ref().unwrap().1, &data, targets[t].entry.chunks[c].offset)
                .with_context(|| format!("could not write {}", path.display()))?;
            progress.done.fetch_add(data.len() as u64, Relaxed);
            progress.elsewhere.fetch_sub(data.len() as u64, Relaxed);
            written.push((t, c));
        }
    }
    log::write(format!("move: {} chunks written in {:.3}s", written.len(), started.elapsed().as_secs_f64()));
    Ok(written)
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
    /// (target, chunk) pairs still to fetch, in file and offset order. A
    /// chunk the package has in several places is here once.
    queue: Vec<(usize, usize)>,
    /// The other places of such a chunk, by its SHA-1.
    twins: HashMap<[u8; 20], Vec<(usize, usize)>>,
    next: AtomicUsize,
    failed: AtomicUsize,
    fatal: Mutex<Option<anyhow::Error>>,
    /// When the progress was last logged.
    reported: Mutex<Instant>,
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
    log::write(format!("download: {output:?}, {} folders, {} files, {jobs} workers, {} servers",
                       folders.len(), targets.len(), servers.len()));
    let create = |folder: &Path| {
        fs::create_dir_all(folder).with_context(|| format!("could not create the folder {}", folder.display()))
    };
    create(output)?;
    progress.log(format!("Output: {}", output.display()));
    let make_folders = || {
        let parents = targets.iter().filter_map(|target| target.path.parent());
        folders.iter().map(PathBuf::as_path).chain(parents).try_for_each(create)
    };
    // Before the new folders are made: one may be where an old file is.
    let (stale, removed) = installed::begin(output, package, progress)?;
    make_folders()?;
    let mut summary = Summary {
        files: targets.len(),
        bytes: targets.iter().map(|target| target.entry.size).sum(),
        ..Summary::default()
    };
    progress.total.store(summary.bytes, Relaxed);
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
    // What an older version has elsewhere is copied before anything is
    // downloaded, as a download may overwrite it.
    progress.local.store(true, Relaxed);
    let mut touched = HashSet::new();
    if find_elsewhere(&targets, &stale, &mut on_disk, progress) {
        let elsewhere = progress.elsewhere.load(Relaxed);
        if elsewhere > 0 {
            progress.log(format!("Another {} is in the folder, but not where this version has it. \
                                  Copying it into place instead of downloading it.", human(elsewhere)));
        }
        for (t, c) in relocate(&targets, &on_disk, progress)? {
            on_disk.add(&targets[t], c);
            touched.insert(t);
        }
    }
    if !progress.stop.load(Relaxed) {
        progress.elsewhere.store(0, Relaxed); // What could not be copied after all is downloaded.
        // Nothing more is wanted from the files this version no longer has.
        // Removing the last file of a folder removes the folder too, so make
        // the new version's folders again.
        installed::finish(output, package, removed, progress)?;
        make_folders()?;
    }
    progress.local.store(false, Relaxed);
    let missing: Vec<(usize, usize)> = targets.iter().enumerate()
        .flat_map(|(t, target)| (0..target.entry.chunks.len()).map(move |c| (t, c)))
        .filter(|&(t, c)| !on_disk.is_good(&targets[t].path, c))
        .collect();
    summary.reused = targets.iter().map(|target| target.entry.chunks.len()).sum::<usize>() - missing.len();
    let mut slots: Vec<Slot> = targets.iter().map(|_| Slot::default()).collect();
    // A chunk missing in several places is fetched once and written to each.
    let (mut queue, mut twins, mut queued) = (Vec::new(), HashMap::<_, Vec<_>>::new(), HashSet::new());
    let mut to_fetch = 0;
    for &(t, c) in &missing {
        let chunk = &targets[t].entry.chunks[c];
        *slots[t].remaining.get_mut() += 1;
        if queued.insert(chunk.sha) {
            queue.push((t, c));
            to_fetch += u64::from(chunk.size);
        } else {
            twins.entry(chunk.sha).or_default().push((t, c));
        }
    }
    log::write(format!("download: {} chunks to fetch ({to_fetch} bytes) for {} places, {} already on disk",
                       queue.len(), missing.len(), summary.reused));
    let work = Work {
        targets: &targets, slots, queue, twins, next: AtomicUsize::new(0), failed: AtomicUsize::new(0),
        fatal: Mutex::new(None), reported: Mutex::new(Instant::now()), agent, servers, progress,
    };
    let fetching = Instant::now();
    let written: Vec<(usize, usize)> = thread::scope(|scope| {
        let work = &work;
        let workers: Vec<_> = (0..jobs.max(1)).map(|id| scope.spawn(move || work.worker(id))).collect();
        workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect()
    });
    let fetch_time = fetching.elapsed();
    drop(work.slots); // Close the files of chunks left unfinished.
    log::write(format!("download: workers done after {:.3}s; {} chunks written, {} failed, {} bytes received, stop {}",
                       fetch_time.as_secs_f64(), written.len(), work.failed.load(Relaxed),
                       progress.received.load(Relaxed), progress.stop.load(Relaxed)));

    if let Some(error) = work.fatal.into_inner().unwrap() {
        return Err(error);
    }
    let failed = work.failed.into_inner();
    ensure!(failed == 0, "{failed} chunks could not be downloaded; try again to fetch them");
    summary.fetched = written.len();
    for (t, c) in written {
        on_disk.add(&targets[t], c);
        touched.insert(t);
    }
    let paused = progress.stop.load(Relaxed);
    if !paused {
        for (t, target) in targets.iter().enumerate() {
            if tidy(target).with_context(|| format!("could not finish {}", target.path.display()))? {
                log::write(format!("download: cut {:?} to {} bytes", target.path, target.entry.size));
                touched.insert(t);
            }
        }
    }
    for t in touched {
        on_disk.restat(&targets[t].path).with_context(|| format!("could not read {}", targets[t].path.display()))?;
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
            // Every place the chunk goes.
            let places = || [(t, c)].into_iter().chain(self.twins.get(&chunk.sha).into_iter().flatten().copied());
            match self.fetch(target, chunk, &mut server) {
                Ok(plain) => for (t, c) in places() {
                    let target = &self.targets[t];
                    match self.slots[t].write(&target.path, &plain, target.entry.chunks[c].offset) {
                        Ok(()) => {
                            self.progress.done.fetch_add(chunk.size.into(), Relaxed);
                            self.progress.fetched.fetch_add(chunk.size.into(), Relaxed);
                            written.push((t, c));
                        }
                        Err(error) => {
                            self.abort(anyhow!(error).context(format!("could not write {}", target.path.display())));
                            break;
                        }
                    }
                },
                Err(_) if self.progress.stop.load(Relaxed) => {}
                Err(error) => {
                    let failed = self.failed.fetch_add(1, Relaxed) + 1;
                    self.progress.log(format!("Chunk of {:?} failed: {error:#}", target.entry.name));
                    if failed >= MAX_FAILURES {
                        self.abort(error.context(format!("giving up after {failed} chunks failed to download")));
                    }
                }
            }
            places().for_each(|(t, _)| self.slots[t].finish_chunk());
            self.report();
        }
        written
    }

    /// Log how far the download has got, if it has not been logged lately.
    fn report(&self) {
        let Ok(mut reported) = self.reported.try_lock() else { return };
        if reported.elapsed() < REPORT_EVERY {
            return;
        }
        *reported = Instant::now();
        let progress = self.progress;
        log::write(format!("download: {} of {} bytes done, {} fetched, {} received; {} of {} chunks started, {} failed",
                           progress.done.load(Relaxed), progress.total.load(Relaxed), progress.fetched.load(Relaxed),
                           progress.received.load(Relaxed), self.next.load(Relaxed).min(self.queue.len()),
                           self.queue.len(), self.failed.load(Relaxed)));
    }

    fn abort(&self, error: anyhow::Error) {
        self.fatal.lock().unwrap().get_or_insert(error);
        self.progress.stop.store(true, Relaxed);
    }

    /// Sleep for `delay`, but wake as soon as the run is stopped.
    fn wait(&self, delay: Duration) {
        let until = Instant::now() + delay;
        while !self.progress.stop.load(Relaxed) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    /// Try each server in turn, three rounds, backing off between rounds.
    fn fetch(&self, target: &Target, chunk: &Chunk, server: &mut usize) -> Result<Vec<u8>> {
        let depot = target.depot;
        let sha: String = chunk.sha.iter().map(|b| format!("{b:02x}")).collect();
        let mut error = anyhow!("no servers");
        for round in 0..3 {
            if round > 0 {
                self.wait(RETRY_DELAY * round);
            }
            for _ in 0..self.servers.len() {
                if self.progress.stop.load(Relaxed) {
                    bail!("stopped");
                }
                let url = format!("{}/depot/{}/chunk/{sha}", self.servers[*server % self.servers.len()], depot.id);
                let attempt = Instant::now();
                let result = self.agent.get(&url).call()
                    .and_then(|mut response| response.body_mut().with_config().limit(64 << 20).read_to_vec())
                    .inspect(|data| _ = self.progress.received.fetch_add(data.len() as u64, Relaxed))
                    .map_err(anyhow::Error::from)
                    .and_then(|data| depot::decode_chunk(&data, &depot.key, chunk));
                match result {
                    Ok(plain) => return Ok(plain),
                    Err(e) => {
                        log::write(format!("chunk failed: {url} ({:?} at {}, {} bytes), round {round}, after {} ms: {e:#}",
                                           target.entry.name, chunk.offset, chunk.size, attempt.elapsed().as_millis()));
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
pub fn destination(root: &Path, name: &str) -> Result<PathBuf> {
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
pub fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, data, offset)
}

#[cfg(windows)]
pub fn read_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
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

/// `YYYY-MM-DD HH:MM:SS UTC` for a Unix time.
pub fn datetime(unix: u64) -> String {
    format!("{} {:02}:{:02}:{:02} UTC", date(unix), unix / 3600 % 24, unix / 60 % 60, unix % 60)
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
