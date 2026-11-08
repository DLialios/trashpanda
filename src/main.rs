//! trashpanda: download Steam depots from a manifest package. Given a
//! package it runs in the terminal; without arguments it opens a window.

mod depot;
mod download;
mod gui;
mod log;
#[cfg(test)]
mod tests;
mod update;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};
use std::{env, thread};

use download::{Meter, Progress};

pub const DEFAULT_JOBS: usize = 16;

const USAGE: &str = "\
Usage: trashpanda PACKAGE.zip [-o DIR] [-j N] [-d ID,ID...] [-l] [--log FILE]
       trashpanda           (opens the window)
       trashpanda --update  (replaces this program with the newest release)

Downloads every depot in PACKAGE.zip (a .lua file with depot keys plus the
depots' .manifest files) into the current directory. Run it again to resume:
chunks already on disk are checked and kept, so it also repairs damaged files.

  -o, --output DIR      download into DIR instead of the current directory
  -j, --jobs N          parallel chunk downloads (default 16)
  -d, --depots ID,ID    download only these depots
  -l, --list            show the app and depots in the package and stop
      --log FILE        save a detailed log to FILE, for bug reports";

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    if args.len() == 0 {
        log::start("window");
        hide_console();
        return match gui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&format!("could not open the window: {error}")),
        };
    }
    log::start("terminal");
    let (mut package, mut output, mut jobs, mut depots, mut list) = (None, None, DEFAULT_JOBS, None, false);
    let mut log_file = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Some("--update") => return self_update(),
            Some("-o" | "--output") => match args.next() {
                Some(dir) => output = Some(PathBuf::from(dir)),
                None => return fail("--output needs a directory"),
            },
            Some("-j" | "--jobs") => match args.next().and_then(|n| n.to_str()?.parse().ok()) {
                Some(n) if n > 0 => jobs = n,
                _ => return fail("--jobs needs a number above 0"),
            },
            Some("-d" | "--depots") => {
                let ids = args.next().and_then(|ids| {
                    ids.to_str()?.split(',').map(|id| id.trim().parse::<u32>().ok()).collect::<Option<Vec<_>>>()
                });
                match ids {
                    Some(ids) => depots = Some(ids),
                    None => return fail("--depots needs depot IDs separated by commas"),
                }
            }
            Some("-l" | "--list") => list = true,
            Some("--log") => match args.next() {
                Some(file) => log_file = Some(log::SaveOnDrop(PathBuf::from(file))),
                None => return fail("--log needs a file"),
            },
            Some(flag) if flag.starts_with('-') => return fail(&format!("unknown option {flag}\n\n{USAGE}")),
            _ if package.is_none() => package = Some(PathBuf::from(arg)),
            _ => return fail("give only one package"),
        }
    }
    let _log_file = log_file; // Saves the log when main returns, however it returns.
    let Some(package) = package else { return fail(&format!("no package given\n\n{USAGE}")) };
    let output = match output.map_or_else(env::current_dir, Ok) {
        Ok(output) => output,
        Err(error) => return fail(&format!("current directory: {error}")),
    };
    let mut package = match depot::read_package(&package) {
        Ok(package) => package,
        Err(error) => return fail(&format!("{error:#}")),
    };
    if let Some(depots) = depots {
        if let Some(missing) = depots.iter().find(|&&id| !package.depots.iter().any(|depot| depot.id == id)) {
            return fail(&format!("depot {missing} is not in the package (see --list)"));
        }
        package.depots.retain(|depot| depots.contains(&depot.id));
    }
    if list {
        download::describe(&package).iter().chain(&package.warnings).for_each(|line| println!("{line}"));
        return ExitCode::SUCCESS;
    }
    log::write(format!("cli: output {output:?}, jobs {jobs}, depots {:?}",
                       package.depots.iter().map(|depot| depot.id).collect::<Vec<_>>()));
    cli(package, output, jobs)
}

fn fail(message: &str) -> ExitCode {
    log::write(format!("Error: {message}"));
    eprintln!("Error: {message}");
    ExitCode::FAILURE
}

/// Replace this program with the newest release, if there is a newer one.
fn self_update() -> ExitCode {
    let updater = update::Updater::new();
    let release = match updater.check() {
        Ok(Some(release)) => release,
        Ok(None) => {
            println!("trashpanda {} is the newest version.", update::current());
            return ExitCode::SUCCESS;
        }
        Err(error) => return fail(&format!("{error:#}")),
    };
    eprintln!("Updating from {} to {}…", update::current(), release.version);
    match updater.install(&release) {
        Ok(_) => {
            println!("Updated to {}.", release.version);
            ExitCode::SUCCESS
        }
        Err(error) => fail(&format!("{error:#}")),
    }
}

/// Run the download on a thread and draw a progress bar on stderr.
fn cli(package: depot::Package, output: PathBuf, jobs: usize) -> ExitCode {
    let progress = Arc::new(Progress::default());
    let task = {
        let progress = progress.clone();
        thread::spawn(move || download::run(&package, &output, jobs, None, &progress))
    };
    let terminal = std::io::stderr().is_terminal();
    let mut meter = Meter::default();
    let mut shown = 0;
    let mut width = 0; // Length of the progress line now on screen.
    let mut last_report = Instant::now();
    loop {
        let finished = task.is_finished();
        let lines = progress.lines(shown);
        shown += lines.len();
        let bar = status_line(&progress, &mut meter);
        let mut err = std::io::stderr().lock();
        for line in lines {
            if terminal { _ = writeln!(err, "\r{line:<width$}") } else { _ = writeln!(err, "{line}") }
        }
        if progress.total.load(Relaxed) > 0 {
            if terminal {
                _ = write!(err, "\r{bar:<width$}");
                width = bar.len();
            } else if finished || last_report.elapsed() >= Duration::from_secs(30) {
                _ = writeln!(err, "{bar}");
                last_report = Instant::now();
            }
        }
        if finished {
            break;
        }
        drop(err);
        thread::sleep(Duration::from_millis(250));
    }
    if terminal {
        eprintln!();
    }
    match task.join() {
        Ok(Ok(_)) => ExitCode::SUCCESS,
        Ok(Err(error)) => fail(&format!("{error:#}")),
        Err(_) => fail("the download thread crashed"),
    }
}

/// `[#######-----------]  37.5%  11.4/30.4 GiB  48.2 MiB/s  6m 15s left`
fn status_line(progress: &Progress, meter: &mut Meter) -> String {
    let rates = meter.update(progress, Instant::now());
    let percent = progress.fraction() * 100.0;
    let filled = ((percent / 5.0) as usize).min(20);
    let done = progress.done.load(Relaxed) as f64 / f64::from(1 << 30);
    let total = progress.total.load(Relaxed) as f64 / f64::from(1 << 30);
    format!("[{}{}] {percent:5.1}%  {done:.1}/{total:.1} GiB  {}  {} left",
            "#".repeat(filled), "-".repeat(20 - filled), rates.speed_text(), rates.left_text())
}

/// A double-clicked Windows program gets a console window of its own; close
/// it for the GUI, but keep the console of a terminal it was started from.
#[cfg(windows)]
fn hide_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut processes = [0u32; 2];
    unsafe {
        if GetConsoleProcessList(processes.as_mut_ptr(), 2) == 1 {
            FreeConsole();
        }
    }
}

#[cfg(not(windows))]
fn hide_console() {}
