//! The list, kept in the output folder, of the files each depot put there.
//! Before a newer version of a depot is downloaded into the same folder, the
//! files on its list that the new manifest leaves out are removed. Files not
//! on the list, such as saves, mods and anything else, are never touched.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::depot::{Depot, Package};
use crate::download::{self, Progress};
use crate::log;

/// The list's file name in the output folder.
pub const FILE: &str = ".trashpanda-installed.json";

const ABOUT: &str = "The files Trash Panda put in this folder, by depot. When a new version of a depot is \
                     downloaded here, the files listed for it that the new version does not have are removed. \
                     Nothing else is ever removed.";

/// For each depot, the files it put in the folder, as paths with `/`.
#[derive(Debug, Default, PartialEq)]
pub struct Installed {
    pub depots: BTreeMap<u32, BTreeSet<String>>,
}

impl Installed {
    /// Read the list in `output`; a folder without one has an empty list.
    pub fn load(output: &Path) -> Result<Self> {
        let path = output.join(FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("could not read {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("{} is damaged", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        let mut installed = Self::default();
        for (id, files) in value["depots"].as_object().context("it lists no depots")? {
            let id = id.parse().with_context(|| format!("{id:?} is not a depot ID"))?;
            let files = files.as_array().with_context(|| format!("depot {id} has no list of files"))?;
            let files = files.iter().map(|file| file.as_str().map(String::from).context("a file name is not text"));
            installed.depots.insert(id, files.collect::<Result<_>>()?);
        }
        Ok(installed)
    }

    /// Write the list into `output`, replacing the old one in a single step.
    pub fn save(&self, output: &Path) -> Result<()> {
        let path = output.join(FILE);
        let failed = || format!("could not save the list of installed files, {}", path.display());
        let depots: serde_json::Map<String, serde_json::Value> = self.depots.iter()
            .map(|(id, files)| (id.to_string(), files.iter().cloned().collect()))
            .collect();
        let text = serde_json::to_string_pretty(&serde_json::json!({ "about": ABOUT, "depots": depots }))? + "\n";
        let temp = output.join(format!("{FILE}.new"));
        _ = fs::remove_file(&temp); // Left by a crash; also never write through a link put there.
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temp).with_context(failed)?;
        file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).with_context(failed)?;
        drop(file);
        fs::rename(&temp, &path).with_context(failed)
    }
}

/// Get `output` ready for `package`: remove the files an earlier download of
/// its depots put there that its manifests no longer have, then list its
/// files. Depots not in `package` keep their files.
pub fn update(output: &Path, package: &Package, progress: &Progress) -> Result<()> {
    let mut installed = Installed::load(output).unwrap_or_else(|error| {
        progress.log(format!("{error:#}. Starting a new list, so files an older version left behind stay."));
        Installed::default()
    });
    log::write(format!("installed: {:?} lists depots {:?}", output.join(FILE), installed.depots.keys()));
    let files = |depot: &Depot| -> BTreeSet<String> {
        depot.entries.iter().filter(|entry| !entry.is_dir()).map(|entry| entry.name.clone()).collect()
    };
    // A file another depot in the package has, which may have moved there, is
    // kept. On a disk that ignores case, an old "Foo.dat" is the new "foo.dat",
    // but removing it only means downloading it again, since this runs first.
    let wanted: HashSet<String> = package.depots.iter().flat_map(files).collect();
    // The files of depots not being downloaded now must stay, whatever case
    // they are written in, as nothing would bring them back.
    let fold = |name: &String| name.to_lowercase();
    let others: HashSet<String> = installed.depots.iter()
        .filter(|(id, _)| !package.depots.iter().any(|depot| depot.id == **id))
        .flat_map(|(_, listed)| listed.iter().map(fold))
        .collect();

    let mut removed = 0;
    for depot in &package.depots {
        let mut listed = files(depot);
        for name in installed.depots.remove(&depot.id).unwrap_or_default() {
            if wanted.contains(&name) || others.contains(&fold(&name)) {
                continue;
            }
            match remove(output, &name, progress) {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(error) => {
                    progress.log(format!("Could not remove {name:?}, which this version no longer has: {error}. \
                                          The next download tries again."));
                    listed.insert(name);
                }
            }
        }
        installed.depots.insert(depot.id, listed);
    }
    if removed > 0 {
        progress.log(format!("Removed {removed} files that an earlier version had and this one does not."));
    }
    installed.save(output)
}

/// Remove the file `name` from `output`, then the folders that leaves empty.
/// Returns whether a file was removed. An error means the file is still there
/// and worth trying again; a name that is unsafe, or no longer a file, is not.
fn remove(output: &Path, name: &str, progress: &Progress) -> io::Result<bool> {
    let path = match download::destination(output, name) {
        Ok(path) => path,
        Err(error) => {
            progress.log(format!("Not removing {name:?}: {error:#}"));
            return Ok(false);
        }
    };
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => fs::remove_file(&path)?,
        Ok(_) => {
            progress.log(format!("Not removing {name:?}: it is no longer a file"));
            return Ok(false);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    log::write(format!("installed: removed {path:?}"));
    // The new version's folders are created after this, so removing one here is harmless.
    let mut folder = path.parent();
    while let Some(dir) = folder.filter(|dir| *dir != output) {
        if fs::remove_dir(dir).is_err() {
            break; // Not empty.
        }
        log::write(format!("installed: removed the empty folder {dir:?}"));
        folder = dir.parent();
    }
    Ok(true)
}
