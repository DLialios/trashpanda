//! The list, kept in the output folder, of the files each depot put there.
//! When a newer version of a depot is downloaded into the same folder, the
//! files on its list that the new manifest leaves out are removed, once the
//! content the new version still uses has been copied out of them. Files not
//! on the list, such as saves, mods and anything else, are never touched.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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

/// The names of a depot's files.
fn files(depot: &Depot) -> impl Iterator<Item = String> + '_ {
    depot.entries.iter().filter(|entry| !entry.is_dir()).map(|entry| entry.name.clone())
}

/// The files `installed` lists for the depots of `package` that its manifests
/// no longer have: the depot, the name, and whether the file is in the way
/// of the new version's own files and folders.
fn dropped(installed: &Installed, package: &Package) -> Vec<(u32, String, bool)> {
    // A file another depot in the package has, which may have moved there, is kept.
    let wanted: HashSet<String> = package.depots.iter().flat_map(files).collect();
    // The files of depots not being downloaded now must stay, whatever case
    // they are written in, as nothing would bring them back.
    let fold = |name: &str| name.to_lowercase();
    let others: HashSet<String> = installed.depots.iter()
        .filter(|(id, _)| !package.depots.iter().any(|depot| depot.id == **id))
        .flat_map(|(_, listed)| listed.iter().map(|name| fold(name)))
        .collect();
    // On a disk that ignores case, an old "Foo.dat" is the new "foo.dat", so
    // names are compared without case here: taking a file for one in the way
    // only means downloading what it might have held.
    let (mut new_files, mut new_folders) = (HashSet::new(), HashSet::new());
    for entry in package.depots.iter().flat_map(|depot| &depot.entries) {
        let name = fold(&entry.name);
        new_folders.extend(name.match_indices('/').map(|(at, _)| name[..at].to_string()));
        if entry.is_dir() { new_folders.insert(name) } else { new_files.insert(name) };
    }
    let in_the_way = |name: &str| {
        let name = fold(name);
        new_files.contains(&name) || new_folders.contains(&name)
            || name.match_indices('/').any(|(at, _)| new_files.contains(&name[..at]))
    };
    let mut dropped = Vec::new();
    for depot in &package.depots {
        for name in installed.depots.get(&depot.id).into_iter().flatten() {
            if !wanted.contains(name) && !others.contains(&fold(name)) {
                dropped.push((depot.id, name.clone(), in_the_way(name)));
            }
        }
    }
    dropped
}

/// The files in `output` that a download of `package` will remove, except
/// those it removes before anything else. Changes nothing.
pub fn leftovers(output: &Path, package: &Package) -> Vec<PathBuf> {
    let installed = Installed::load(output).unwrap_or_default();
    let late = dropped(&installed, package).into_iter().filter(|(_, _, in_the_way)| !in_the_way);
    late.filter_map(|(_, name, _)| existing(output, &name)).collect()
}

/// The path of the file `name` in `output`, if it is safe and there is one.
fn existing(output: &Path, name: &str) -> Option<PathBuf> {
    let path = download::destination(output, name).ok()?;
    fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()).then_some(path)
}

/// Get `output` ready for `package`: list its files, and remove the files
/// that an earlier download of its depots put there and that are in the way
/// of the new version's. Returns the other files of the earlier version that
/// the new one no longer has, which `finish` removes: until then the download
/// may copy content out of them. Also returns how many files it removed.
/// Depots not in `package` keep their files.
pub fn begin(output: &Path, package: &Package, progress: &Progress) -> Result<(Vec<PathBuf>, usize)> {
    sweep(output, package, progress, false, 0)
}

/// Remove the files `begin` left, and say how many were removed, counting
/// the `before` that `begin` removed.
pub fn finish(output: &Path, package: &Package, before: usize, progress: &Progress) -> Result<()> {
    sweep(output, package, progress, true, before).map(drop)
}

fn sweep(output: &Path, package: &Package, progress: &Progress, all: bool, before: usize) -> Result<(Vec<PathBuf>, usize)> {
    let mut installed = Installed::load(output).unwrap_or_else(|error| {
        progress.log(format!("{error:#}. Starting a new list, so files an older version left behind stay."));
        Installed::default()
    });
    log::write(format!("installed: {:?} lists depots {:?}", output.join(FILE), installed.depots.keys()));
    let dropped = dropped(&installed, package);
    for depot in &package.depots {
        installed.depots.insert(depot.id, files(depot).collect());
    }
    let (mut kept, mut removed) = (Vec::new(), before);
    for (depot, name, in_the_way) in dropped {
        // Whatever stays on disk stays on the list, for a later run to remove.
        let listed = installed.depots.entry(depot).or_default();
        if !all && !in_the_way {
            kept.extend(existing(output, &name));
            listed.insert(name);
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
    if all && removed > 0 {
        progress.log(format!("Removed {removed} files that an earlier version had and this one does not."));
    }
    installed.save(output)?;
    Ok((kept, removed))
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
    // The download makes the new version's folders after this, so removing one here is harmless.
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
