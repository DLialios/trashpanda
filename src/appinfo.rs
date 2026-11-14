//! Steam's record of an app: its name, and the tags on each depot that say
//! who the depot is for. The package does not hold the
//! record, and Steam serves it only over its own client protocol, so it
//! comes from a third-party mirror that serves it as JSON to anyone.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::depot::Package;
use crate::glimpse::Platform;
use crate::select::Tags;
use crate::{download, log};

/// `<MIRROR>/<appid>` answers `{"status": "success", "data": {"<appid>": {...}}}`.
pub const MIRROR: &str = "https://api.steamcmd.net/v1/info";
/// The tags under a depot's `config` that this program knows.
const KNOWN: [&str; 5] = ["oslist", "osarch", "language", "lowviolence", "optionaldlc"];
/// The most DLC whose names are looked up for one package, one request each.
const NAMES: usize = 64;
/// Largest answer accepted. Train Simulator's record, with 1,320 DLC, is 280 KiB.
const LIMIT: u64 = 16 << 20;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub name: Option<String>,
    /// The tags of each depot the record lists.
    pub depots: BTreeMap<u32, Tags>,
}

impl Record {
    /// The DLC that `depots` belong to, in ID order.
    pub fn dlc(&self, depots: &[u32]) -> Vec<u32> {
        let mut dlc: Vec<u32> = depots.iter().filter_map(|id| self.depots.get(id)?.dlc).collect();
        dlc.sort();
        dlc.dedup();
        dlc
    }
}

/// Read the mirror's answer for `app`. Every value in it is a string, and
/// any of them may be missing; a record with nothing in it is no record.
pub fn parse(json: &str, app: u32) -> Result<Record> {
    let value: serde_json::Value = serde_json::from_str(json).context("the answer is not JSON")?;
    let record = &value["data"][app.to_string()];
    if value["status"] != "success" || record.as_object().is_none_or(|record| record.is_empty()) {
        bail!("the mirror has no record of app {app}");
    }
    fn text(value: &serde_json::Value) -> Option<&str> {
        value.as_str().map(str::trim).filter(|text| !text.is_empty())
    }
    let number = |value: &serde_json::Value| text(value)?.parse::<u64>().ok();
    let mut depots = BTreeMap::new();
    // `depots` also holds `branches` and other keys that are not depots.
    for (id, depot) in record["depots"].as_object().into_iter().flatten() {
        let Some(id) = id.bytes().all(|byte| byte.is_ascii_digit()).then(|| id.parse().ok()).flatten() else { continue };
        let config = &depot["config"];
        let dlc = number(&depot["dlcappid"]).or_else(|| number(&config["optionaldlc"]));
        let tags = Tags {
            systems: text(&config["oslist"]).into_iter().flat_map(|list| list.split(','))
                .filter_map(|system| Platform::from_steam(system.trim())).collect(),
            arch: text(&config["osarch"]).map(String::from),
            language: text(&config["language"]).map(String::from),
            low_violence: text(&config["lowviolence"]) == Some("1"),
            // The app's own ID marks no DLC: whoever has the app has it.
            dlc: dlc.and_then(|dlc| u32::try_from(dlc).ok()).filter(|&dlc| dlc != app),
            other: config.as_object().into_iter().flatten().filter(|(kind, _)| !KNOWN.contains(&kind.as_str()))
                .filter_map(|(kind, value)| Some((kind.clone(), text(value)?.to_string()))).collect(),
        };
        depots.insert(id, tags);
    }
    Ok(Record { name: text(&record["common"]["name"]).map(String::from), depots })
}

/// The app's record and its DLC's names from the package itself, if it
/// carries them: such a package needs no mirror.
pub fn bundled(package: &Package) -> Option<(Record, HashMap<u32, String>)> {
    let app = package.app?;
    let record = parse(package.records.get(&app)?, app)
        .inspect_err(|error| log::write(format!("appinfo: the package's own record of app {app} is no use: {error:#}"))).ok()?;
    let depots: Vec<u32> = package.depots.iter().map(|depot| depot.id).collect();
    let name = |dlc: u32| Some((dlc, parse(package.records.get(&dlc)?, dlc).ok()?.name?));
    let names: HashMap<u32, String> = record.dlc(&depots).into_iter().filter_map(name).collect();
    log::write(format!("appinfo: app {app} is {:?} by the package's own record: {} depots, {} DLC names",
                       record.name, record.depots.len(), names.len()));
    Some((record, names))
}

/// The mirror at `base`.
pub struct Mirror {
    base: String,
    agent: ureq::Agent,
}

impl Mirror {
    pub fn new(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').into(), agent: download::agent(1) }
    }

    pub fn record(&self, app: u32) -> Result<Record> {
        let url = format!("{}/{app}", self.base);
        let started = Instant::now();
        log::write(format!("appinfo: GET {url}"));
        let record = (|| {
            let mut response = self.agent.get(&url).call()?;
            parse(&response.body_mut().with_config().limit(LIMIT).read_to_string()?, app)
        })();
        match &record {
            Ok(record) => log::write(format!("appinfo: app {app} is {:?}: {} depots, in {:.1?}",
                                             record.name, record.depots.len(), started.elapsed())),
            Err(error) => log::write(format!("appinfo: app {app}: {error:#}")),
        }
        record.with_context(|| format!("could not look up app {app}"))
    }
}

/// What a lookup found.
#[derive(Debug)]
pub enum Found {
    /// The app's record, or None if it could not be had.
    Record(Option<Record>),
    /// The name of a DLC.
    Name(u32, String),
}

/// Looking up an app and its DLC's names on a thread of its own. Dropping
/// it stops the thread after the request in flight, and nothing that
/// request finds can reach anyone.
pub struct Lookup {
    stop: Arc<AtomicBool>,
    found: Receiver<Found>,
    done: bool,
}

impl Lookup {
    /// Look up `app` on the mirror at `base`, then the names of the DLC that
    /// `depots` belong to, one at a time.
    pub fn start(base: &str, app: u32, depots: Vec<u32>) -> Self {
        let (send, found) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (mirror, stopped) = (Mirror::new(base), stop.clone());
        thread::spawn(move || {
            let record = mirror.record(app).ok();
            let dlc = record.as_ref().map(|record| record.dlc(&depots)).unwrap_or_default();
            if stopped.load(Relaxed) || send.send(Found::Record(record)).is_err() {
                return;
            }
            if dlc.len() > NAMES {
                log::write(format!("appinfo: looking up the names of only {NAMES} of {} DLC", dlc.len()));
            }
            for dlc in dlc.into_iter().take(NAMES) {
                if stopped.load(Relaxed) {
                    log::write(format!("appinfo: the package changed; no more lookups for app {app}"));
                    return;
                }
                if let Ok(Record { name: Some(name), .. }) = mirror.record(dlc)
                    && send.send(Found::Name(dlc, name)).is_err()
                {
                    return;
                }
            }
        });
        Self { stop, found, done: false }
    }

    /// What has been found since the last call.
    pub fn found(&mut self) -> Vec<Found> {
        let mut found = Vec::new();
        loop {
            match self.found.try_recv() {
                Ok(item) => found.push(item),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
            }
        }
        found
    }

    /// Whether the lookup has ended and everything it found has been taken.
    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl Drop for Lookup {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}
