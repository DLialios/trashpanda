//! The systems a depot is for, guessed from the names of its programs and
//! libraries: what the window goes by when Steam's record of the app cannot
//! be had.

use crate::depot::Depot;

/// In the order the window offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Platform {
    Windows,
    Mac,
    Linux,
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Platform::Windows => "Windows",
            Platform::Mac => "macOS",
            Platform::Linux => "Linux",
        }
    }

    /// The system an app's record calls `name` in a depot's `oslist`.
    pub fn from_steam(name: &str) -> Option<Self> {
        match name {
            "windows" => Some(Platform::Windows),
            "macos" => Some(Platform::Mac),
            "linux" => Some(Platform::Linux),
            _ => None,
        }
    }

    /// The system this program runs on.
    pub fn here() -> Option<Self> {
        match std::env::consts::OS {
            "windows" => Some(Platform::Windows),
            "macos" => Some(Platform::Mac),
            "linux" => Some(Platform::Linux),
            _ => None,
        }
    }
}

/// The system a file is a program or library for, and whether it is a
/// library, if its name shows that. Steam's executable flag is no help: of
/// 839 manifests one Steam client had, only two set it on anything.
fn mark(name: &str) -> Option<(Platform, bool)> {
    let name = name.to_ascii_lowercase();
    let file = name.rsplit('/').next().unwrap_or(&name);
    let ends = |extension: &str| file.ends_with(extension);
    // libfoo.so.1.2
    let versioned = |file: &str| file.rsplit_once(".so.").is_some_and(|(_, version)| {
        version.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
    });
    Some(match () {
        _ if ends(".dylib") => (Platform::Mac, true),
        _ if name.contains(".app/contents/macos/") => (Platform::Mac, false),
        _ if ends(".so") || versioned(file) => (Platform::Linux, true),
        _ if ends(".x86_64") || ends(".x86") || ends(".appimage") => (Platform::Linux, false),
        _ if ends(".exe") => (Platform::Windows, false),
        _ if ends(".dll") => (Platform::Windows, true),
        _ => return None,
    })
}

/// The systems a depot has programs or libraries for; none if it is only data.
pub fn systems(depot: &Depot) -> Vec<Platform> {
    let marked: Vec<_> = depot.entries.iter().filter(|entry| !entry.is_dir()).filter_map(|entry| mark(&entry.name)).collect();
    let has = |platform, library| marked.contains(&(platform, library));
    // Games for every system ship .dll files, as Unity's scripts are, so
    // a .dll shows Windows only where nothing shows another system.
    let unix = [Platform::Mac, Platform::Linux].map(|platform| has(platform, false) || has(platform, true));
    let windows = has(Platform::Windows, false) || (has(Platform::Windows, true) && unix == [false, false]);
    [(Platform::Windows, windows), (Platform::Mac, unix[0]), (Platform::Linux, unix[1])]
        .into_iter().filter_map(|(platform, found)| found.then_some(platform)).collect()
}
