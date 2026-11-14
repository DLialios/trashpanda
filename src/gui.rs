//! The window: choose a package and an output folder, then press Download.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui;

use crate::appinfo::{self, Found, Lookup, Record};
use crate::depot::{self, Depot, Package};
use crate::download::{self, Meter, OnDisk, Progress};
use crate::glimpse::Platform;
use crate::log;
use crate::select::{self, Choices, Offer, Selector, Tags};
use crate::update::{self, Release, Unwritable, Updater, Version};

const TITLE: &str = "Trash Panda";
/// How far inside the main window's edges the log and the depots open.
const MODAL_MARGIN: f32 = 48.0;
/// The width of the path fields' labels.
const LABEL: f32 = 96.0;
/// A kind of tag with this many values or more is a list rather than tiles.
const LIST_FROM: usize = 12;
const TILE_HEIGHT: f32 = 52.0;
/// The space between tiles, across and down.
const TILE_GAP: f32 = 8.0;
const ROW_HEIGHT: f32 = 36.0;
/// How many rows a list shows before it scrolls, unless it is the last and
/// there is room for more.
const LIST_ROWS: f32 = 10.0;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_icon(icon())
            .with_inner_size([1000.0, 700.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(TITLE, options, Box::new(|creation| {
        creation.egui_ctx.set_theme(egui::Theme::Dark);
        set_fonts(&creation.egui_ctx);
        let mut app = App::default();
        app.check_for_update(true);
        Ok(Box::new(app))
    }))
}

/// The name of the font family for headings and big numbers.
const BOLD: &str = "bold";

/// Use Noto Sans for text, and its semibold as the `BOLD` family. egui's own
/// fonts stay behind both, for the symbols Noto Sans lacks.
fn set_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut add = |name: &str, font: &'static [u8], family: egui::FontFamily| {
        fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(font)));
        let mut names = vec![name.to_string()];
        names.extend(fonts.families.get(&egui::FontFamily::Proportional).into_iter().flatten().cloned());
        fonts.families.insert(family, names);
    };
    add("semibold", include_bytes!("../assets/fonts/NotoSans-SemiBold.ttf"), egui::FontFamily::Name(BOLD.into()));
    add("regular", include_bytes!("../assets/fonts/NotoSans-Regular.ttf"), egui::FontFamily::Proportional);
    ctx.set_fonts(fonts);
}

/// Text in the semibold font and the brightest colour.
fn bold(text: impl Into<String>, size: f32) -> egui::RichText {
    egui::RichText::new(text).size(size).family(egui::FontFamily::Name(BOLD.into())).strong()
}

/// The icon of the window, which the title bar and taskbar show. eframe
/// scales it to each size; on Windows the .exe also carries hand-drawn small
/// sizes for Explorer (see build.rs).
pub fn icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("assets/icon.png is a PNG")
}

/// What is chosen: the package path and output folder as typed, and the
/// depots left out.
#[derive(Clone, Default, PartialEq)]
struct Choice {
    package: String,
    output: String,
    skip: BTreeSet<u32>,
}

pub struct App {
    package: String,
    output: String,
    /// Depots not to download.
    skip: BTreeSet<u32>,
    /// The package path last read for display, and what reading it gave.
    info: Option<(String, Result<Arc<Package>, String>)>,
    picker: Picker,
    /// Where Steam's records of apps come from.
    mirror: String,
    /// The choice whose state the progress bar shows.
    shown: Choice,
    /// What is on disk for a choice, from the last check or download of it.
    on_disk: Option<(Choice, Arc<OnDisk>)>,
    progress: Arc<Progress>,
    meter: Meter,
    task: Option<Task>,
    update: Update,
    /// A file dialog that is open, and what it is for. It runs on a thread
    /// of its own: while the window's thread waited for it, the window
    /// answered nothing, and the system took it for frozen.
    picking: Option<(Pick, JoinHandle<Option<PathBuf>>)>,
    /// Whether the log window is open.
    log_open: bool,
    /// What last went wrong with a check, a download or saving the log, in
    /// a few words; the log has the details.
    error: Option<String>,
}

/// Checking for a newer version, and installing it.
enum Update {
    /// Not checked yet, or a quiet check found nothing worth showing.
    Idle,
    /// A quiet check, made when the window opens, shows only a newer version.
    Checking { quiet: bool, job: JoinHandle<anyhow::Result<Option<Release>>> },
    Current,
    Available(Release),
    Installing { version: Version, job: JoinHandle<anyhow::Result<PathBuf>> },
    /// The new version is installed and started; this window is closing.
    Restarting,
    /// What went wrong, and the release page if the update has to be
    /// downloaded by hand.
    Failed { message: String, page: Option<String> },
}

/// What a file dialog is open for.
enum Pick {
    Package,
    Output,
    /// Where to save the log.
    Log,
}

/// A check or a download running on its own thread.
struct Task {
    download: bool,
    choice: Choice,
    handle: JoinHandle<anyhow::Result<Option<OnDisk>>>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            package: String::new(),
            output: String::new(),
            skip: BTreeSet::new(),
            info: None,
            picker: Picker::default(),
            mirror: appinfo::MIRROR.into(),
            shown: Choice::default(),
            on_disk: None,
            progress: Arc::default(),
            meter: Meter::default(),
            task: None,
            update: Update::Idle,
            picking: None,
            log_open: false,
            error: None,
        }
    }
}

impl App {
    fn choice(&self) -> Choice {
        Choice { package: self.package.trim().into(), output: self.output.trim().into(), skip: self.skip.clone() }
    }

    fn parsed(&self) -> Option<Arc<Package>> {
        match &self.info {
            Some((_, Ok(package))) => Some(package.clone()),
            _ => None,
        }
    }

    /// The package with only the depots ticked, if any are.
    fn selected(&self) -> Option<Arc<Package>> {
        let package = self.parsed()?;
        let package = if self.skip.is_empty() { package } else { Arc::new(package.select(|id| !self.skip.contains(&id))) };
        Some(package).filter(|package| !package.depots.is_empty())
    }

    /// Read the package again whenever its path changes, to show what is in it.
    fn read_info(&mut self) {
        let path = self.package.trim();
        if path.is_empty() {
            self.info = None;
            self.picker = Picker::default();
        } else if self.info.as_ref().is_none_or(|(read, _)| read != path) {
            log::write(format!("gui: package {path:?}"));
            let package = depot::read_package(Path::new(path)).map(Arc::new).map_err(|error| {
                log::write(format!("gui: cannot use the package: {error:#}"));
                log::brief(&error)
            });
            // A new picker drops the lookup for the package before, which
            // then can no longer change anything.
            self.picker = match &package {
                Ok(package) => Picker::new(package, Some(&self.mirror)),
                Err(_) => Picker::default(),
            };
            self.skip = self.picker.skip();
            self.info = Some((path.to_string(), package));
        }
    }

    /// Take what the lookup of the app's record has found. The record
    /// changes what is chosen, so it waits while a download runs.
    fn collect_lookup(&mut self, downloading: bool) {
        if let (false, Some(package)) = (downloading, self.parsed())
            && self.picker.collect(&package)
        {
            self.skip = self.picker.skip();
        }
    }

    /// Run `job` on a thread and show its progress.
    fn spawn(&mut self, download: bool, job: impl FnOnce(&Progress) -> anyhow::Result<Option<OnDisk>> + Send + 'static) {
        self.error = None;
        self.progress = Arc::default();
        self.meter = Meter::default();
        self.shown = self.choice();
        let progress = self.progress.clone();
        let handle = thread::spawn(move || job(&progress));
        self.task = Some(Task { download, choice: self.choice(), handle });
    }

    /// What the last check or download found for this package and folder, so
    /// that files unchanged since are not read again.
    fn previous(&self) -> Option<Arc<OnDisk>> {
        let same = |checked: &Choice| (&checked.package, &checked.output) == (&self.package.trim().into(), &self.output.trim().into());
        self.on_disk.as_ref().filter(|(checked, _)| same(checked)).map(|(_, on_disk)| on_disk.clone())
    }

    fn start(&mut self) {
        let Some(package) = self.selected() else { return };
        let (output, jobs, previous) = (PathBuf::from(self.output.trim()), crate::DEFAULT_JOBS, self.previous());
        log::write(format!("gui: download {:?} into {output:?}, skipping depots {:?}, {jobs} jobs, previous check {}",
                           self.package.trim(), self.skip, previous.is_some()));
        self.spawn(true, move |progress| {
            download::run(&package, &output, jobs, previous.as_deref(), progress).map(|summary| summary.on_disk)
        });
    }

    /// When the package, folder or depots change, find out how much is already
    /// downloaded there.
    fn check(&mut self) {
        if self.choice() == self.shown {
            return;
        }
        if self.task.is_some() {
            if !self.progress.stop.swap(true, Relaxed) {
                log::write("gui: the choice changed; stopping the check of the earlier one");
            }
            return; // Check again once it stops.
        }
        let output = PathBuf::from(self.output.trim());
        let (Some(package), true) = (self.selected(), output.is_dir()) else {
            self.progress = Arc::default();
            self.shown = self.choice();
            return;
        };
        let previous = self.previous();
        log::write(format!("gui: check {:?} in {output:?}, skipping depots {:?}, previous check {}",
                           self.package.trim(), self.skip, previous.is_some()));
        self.spawn(false, move |progress| download::check(&package, &output, previous.as_deref(), progress));
    }

    /// Keep what a finished task found on disk, or say what went wrong.
    fn collect(&mut self) {
        if self.task.as_ref().is_some_and(|task| task.handle.is_finished()) {
            let task = self.task.take().unwrap();
            let kind = if task.download { "download" } else { "check" };
            match task.handle.join() {
                Ok(Ok(on_disk)) => {
                    log::write(format!("gui: {kind} ended{}", if on_disk.is_some() { "" } else { " early" }));
                    if let Some(on_disk) = on_disk {
                        self.on_disk = Some((task.choice, Arc::new(on_disk)));
                    }
                }
                Ok(Err(error)) => {
                    log::write(format!("gui: {kind} failed: {error:#}"));
                    self.error = Some(log::brief(&error));
                }
                Err(_) => {
                    log::write(format!("gui: {kind} crashed"));
                    self.error = Some(format!("The {kind} crashed. This is a bug: please send the log \
                                               (the log button at the top right, then Export log…)."));
                }
            }
        }
    }

    /// Open a file dialog for `pick`, unless one is open.
    fn pick(&mut self, ctx: &egui::Context, pick: Pick) {
        if self.picking.is_some() {
            return;
        }
        let dialog = rfd::FileDialog::new();
        let start = self.output.trim().to_string();
        let ctx = ctx.clone();
        let run = move |dialog: Box<dyn FnOnce() -> Option<PathBuf> + Send>| thread::spawn(move || {
            let path = dialog();
            ctx.request_repaint(); // Wake the window to take the answer.
            path
        });
        let job = match pick {
            Pick::Package => run(Box::new(move || dialog.add_filter("Package", &["zip"]).pick_file())),
            Pick::Output if start.is_empty() => run(Box::new(move || dialog.pick_folder())),
            Pick::Output => run(Box::new(move || dialog.set_directory(start).pick_folder())),
            Pick::Log => run(Box::new(move || dialog.set_file_name("trashpanda-log.txt").save_file())),
        };
        self.picking = Some((pick, job));
    }

    /// Take the answer of a file dialog that has closed.
    fn collect_pick(&mut self) {
        if self.picking.as_ref().is_none_or(|(_, job)| !job.is_finished()) {
            return;
        }
        let (pick, job) = self.picking.take().unwrap();
        let Ok(Some(path)) = job.join() else { return }; // Cancelled.
        match pick {
            Pick::Package => self.package = path.display().to_string(),
            Pick::Output => self.output = path.display().to_string(),
            Pick::Log => {
                log::write(format!("gui: saving the log to {path:?}"));
                if let Err(error) = log::save(&path).context("could not save the log") {
                    log::write(format!("gui: {error:#}"));
                    self.error = Some(log::brief(&error));
                }
            }
        }
    }

    fn check_for_update(&mut self, quiet: bool) {
        log::write(format!("gui: checking for updates{}", if quiet { " quietly" } else { "" }));
        self.update = Update::Checking { quiet, job: thread::spawn(|| Updater::new().check()) };
    }

    fn install_update(&mut self, release: Release) {
        let version = release.version;
        log::write(format!("gui: updating to {version}"));
        self.update = Update::Installing { version, job: thread::spawn(move || Updater::new().install(&release)) };
    }

    /// Take the result of a finished update check or install. After an
    /// install, start the new version and close this window.
    fn collect_update(&mut self, ctx: &egui::Context) {
        let failed = |message: String| Update::Failed { message, page: None };
        self.update = match std::mem::replace(&mut self.update, Update::Idle) {
            Update::Checking { quiet, job } if job.is_finished() => match job.join() {
                Ok(Ok(Some(release))) => Update::Available(release),
                Ok(Ok(None)) if !quiet => Update::Current,
                Ok(Ok(None)) => Update::Idle,
                Ok(Err(error)) => {
                    log::write(format!("gui: update check failed: {error:#}"));
                    if quiet { Update::Idle } else { failed(log::brief(&error)) }
                }
                Err(_) => {
                    log::write("gui: the update check crashed");
                    if quiet { Update::Idle } else { failed("The update check crashed.".into()) }
                }
            },
            Update::Installing { version, job } if job.is_finished() => match job.join() {
                Ok(Ok(exe)) => match update::restart(&exe) {
                    Ok(()) => {
                        log::write(format!("gui: started {exe:?}; closing this window"));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        Update::Restarting
                    }
                    Err(error) => {
                        log::write(format!("gui: updated to {version}, but could not start {exe:?}: {error}"));
                        failed(format!("Updated to {version}. Close this window and start Trash Panda again."))
                    }
                },
                Ok(Err(error)) => {
                    log::write(format!("gui: update failed: {error:#}"));
                    match error.downcast_ref::<Unwritable>() {
                        Some(unwritable) => Update::Failed {
                            message: format!("Cannot update: {} cannot be written to. Download the new version \
                                              by hand.", unwritable.dir.display()),
                            page: Some(unwritable.page.clone()),
                        },
                        None => failed(log::brief(&error)),
                    }
                }
                Err(_) => {
                    log::write("gui: the update crashed");
                    failed("The update crashed.".into())
                }
            },
            state => state,
        };
    }

    /// The update button and, to its left, what it found, laid out right to
    /// left. Pressing it checks for a newer version, or installs one found.
    fn update_controls(&mut self, ui: &mut egui::Ui, downloading: bool) {
        let pressed = match &self.update {
            Update::Available(release) => {
                // Why it cannot be installed now: in a few words, and in full.
                let blocked = if !update::REPLACES_ITSELF {
                    Some(("Debug build", "A debug build is never replaced by an update"))
                } else if downloading {
                    Some(("Pause to update", "Pause the download first; Resume continues it afterwards"))
                } else {
                    None
                };
                let button = ui.add_enabled(blocked.is_none(), icon_button(Icon::Update, true));
                let pressed = button.on_hover_text(format!("Install version {}", release.version))
                    .on_disabled_hover_text(blocked.map_or("", |(_, why)| why)).clicked();
                if let Some((label, _)) = blocked {
                    ui.weak(label);
                }
                ui.hyperlink_to(format!("Version {} is out", release.version), &release.page)
                    .on_hover_text("What is new in it");
                pressed
            }
            state => {
                let words = match state {
                    Update::Checking { quiet: false, .. } => Some("Checking for updates…".to_string()),
                    Update::Current => Some("Up to date".into()),
                    Update::Installing { version, .. } => Some(format!("Updating to {version}…")),
                    Update::Restarting => Some("Restarting…".into()),
                    _ => None,
                };
                // Words mean it is busy, or has said what it found.
                let busy = words.is_some() && !matches!(state, Update::Current);
                let pressed = ui.add_enabled(!busy, icon_button(Icon::Update, false)).on_hover_text("Check for updates").clicked();
                if let Some(words) = words {
                    ui.label(words);
                }
                pressed
            }
        };
        if pressed {
            match &mut self.update {
                Update::Available(release) => {
                    let release = release.clone();
                    self.install_update(release);
                }
                Update::Checking { quiet, .. } => *quiet = false, // Show the quiet check instead.
                _ => self.check_for_update(false),
            }
        }
    }

    /// The progress in big type with the Pause button, or before a download,
    /// how much there is to download with the Download, Resume or Patch button.
    fn readout(&mut self, ui: &mut egui::Ui, downloading: bool, checking: bool, updating: bool) {
        let (done, total) = (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed));
        let fraction = self.progress.fraction();
        let percent = format!("{:.1}%", fraction * 100.0);
        let big = |text: String| bold(text, 64.0);
        let bar = |ui: &mut egui::Ui| ui.add(egui::ProgressBar::new(fraction).desired_height(14.0).corner_radius(7.0));
        if downloading {
            let rates = self.meter.update(&self.progress, Instant::now());
            row(ui, egui::Align::Max, |ui| {
                ui.label(big(percent));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                    ui.label(egui::RichText::new("left").size(22.0).weak());
                    ui.label(bold(rates.left_text(), 40.0));
                });
            });
            ui.add_space(6.0);
            bar(ui);
            ui.add_space(8.0);
            row(ui, egui::Align::Center, |ui| {
                let (done, total) = (download::human(done), download::human(total));
                // Before it downloads, it copies what an older version has in other places.
                let pace = if self.progress.local.load(Relaxed) { "using what is already here".into() } else { rates.speed_text() };
                ui.label(egui::RichText::new(format!("{done} of {total}   ·   {pace}")).size(16.0).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pausing = self.progress.stop.load(Relaxed);
                    if big_button(ui, if pausing { "Pausing…" } else { "Pause" }, false, !pausing).clicked() {
                        log::write("gui: pause");
                        self.progress.stop.store(true, Relaxed);
                    }
                });
            });
            return;
        }
        let started = done > 0 && total > 0;
        let package = self.parsed();
        let chosen = package.iter().flat_map(|package| &package.depots).filter(|depot| !self.skip.contains(&depot.id));
        let size = chosen.map(depot::Depot::size).reduce(|a, b| a + b); // None when no depot is ticked.
        let no_output = self.output.trim().is_empty();
        // What an older version has in other places is copied, not downloaded.
        let elsewhere = self.progress.elsewhere.load(Relaxed);
        let (amount, note) = match size {
            _ if started && elsewhere > 0 => (percent, format!("downloaded\n{} of {}\nand {} more only has to be moved",
                                                               download::human(done), download::human(total),
                                                               download::human(elsewhere))),
            _ if started => (percent, format!("downloaded\n{} of {}", download::human(done), download::human(total))),
            Some(size) => (download::human(size.saturating_sub(elsewhere)), "to download".into()),
            None => (String::new(), String::new()),
        };
        row(ui, egui::Align::Center, |ui| {
            let number = ui.label(big(amount)).rect;
            let note = if checking { "checking what is\nalready downloaded…".into() } else { note };
            // Beside the foot of the number, from the bottom up: the space is
            // about what the big digits leave under their baseline.
            let foot = egui::Layout::bottom_up(egui::Align::Min);
            ui.allocate_ui_with_layout(egui::vec2(260.0, number.height()), foot, |ui| {
                ui.add_space(12.0);
                if let Some(package) = &package {
                    let chosen = package.depots.iter().filter(|depot| !self.skip.contains(&depot.id)).count();
                    let link = format!("{chosen} of {} depots", package.depots.len());
                    if ui.link(egui::RichText::new(link).size(14.0)).on_hover_text("Show the depots").clicked() {
                        self.picker.list_open = !self.picker.list_open;
                    }
                }
                ui.label(egui::RichText::new(note).size(16.0).weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = Next::of(checking, self.progress.update.load(Relaxed), done, total).label();
                let ready = !checking && !updating && size.is_some() && !no_output;
                if big_button(ui, label, true, ready).clicked() {
                    self.start();
                }
            });
        });
        if started {
            ui.add_space(6.0);
            bar(ui);
        }
    }

    /// The session log in a window of its own, with a button to save it.
    fn show_log(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }
        let lines = log::lines();
        let rows: Vec<&str> = lines.iter().flat_map(|line| line.lines()).collect();
        let mut export = false;
        modal(ctx, "Log")
            .open(&mut self.log_open)
            .show(ctx, |ui| {
                let button = ui.add_enabled(self.picking.is_none(), egui::Button::new("Export log…"));
                export = button.on_hover_text("Save the log to a text file, for a bug report").clicked();
                egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    // Each row on one line, so that only the rows in view need laying out.
                    let height = ui.text_style_height(&egui::TextStyle::Monospace);
                    let scroll = egui::ScrollArea::both().stick_to_bottom(true).auto_shrink(false);
                    scroll.show_rows(ui, height, rows.len(), |ui, range| {
                        for row in &rows[range] {
                            ui.add(egui::Label::new(egui::RichText::new(*row).monospace()).extend());
                        }
                    });
                });
            });
        if export {
            self.pick(ctx, Pick::Log);
        }
    }

    /// The depots in a window of its own, in ID order, each with its tags
    /// and a box to tick it by hand.
    fn show_depots(&mut self, ctx: &egui::Context, editable: bool) {
        let Some(package) = self.parsed().filter(|_| self.picker.list_open) else { return };
        let (picker, skip) = (&mut self.picker, &mut self.skip);
        let mut open = true;
        modal(ctx, "Depots")
            .open(&mut open)
            .show(ctx, |ui| {
                row(ui, egui::Align::Center, |ui| {
                    let chosen = package.depots.iter().filter(|depot| !skip.contains(&depot.id)).count();
                    ui.weak(format!("{chosen} of {} depots chosen", package.depots.len()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.selectable_value(&mut picker.list_all, true, "All");
                        ui.selectable_value(&mut picker.list_all, false, "Chosen");
                    });
                });
                let mut depots: Vec<&Depot> =
                    package.depots.iter().filter(|depot| picker.list_all || !skip.contains(&depot.id)).collect();
                depots.sort_by_key(|depot| depot.id);
                egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                    let height = ui.spacing().interact_size.y;
                    let scroll = egui::ScrollArea::vertical().auto_shrink(false);
                    scroll.show_rows(ui, height, depots.len(), |ui, range| {
                        for depot in &depots[range] {
                            depot_row(ui, depot, picker, skip, editable);
                        }
                    });
                });
            });
        self.picker.list_open = open;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect();
        self.collect_update(ui.ctx());
        self.collect_pick();
        let downloading = self.task.as_ref().is_some_and(|task| task.download);
        let updating = matches!(self.update, Update::Installing { .. } | Update::Restarting);
        let dropped = ui.input(|input| input.raw.dropped_files.first().map(|file| file.path().to_path_buf()));
        if let (false, Some(path)) = (downloading, dropped) {
            log::write(format!("gui: file dropped: {path:?}"));
            self.package = path.display().to_string();
        }
        self.read_info();
        self.collect_lookup(downloading);
        self.check();
        let checking = self.task.as_ref().is_some_and(|task| !task.download);
        let (done, total) = (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed));
        let next = Next::of(checking, self.progress.update.load(Relaxed), done, total);
        let press = what_to_press(next, !self.output.trim().is_empty(), downloading);
        egui::Panel::bottom("readout").show(ui, |ui| {
            ui.add_space(10.0);
            self.readout(ui, downloading, checking, updating);
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            row(ui, egui::Align::Max, |ui| {
                ui.label(bold(TITLE, 30.0).italics());
                ui.label(egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).weak());
                // From the right: the log, then updates, whose words have the bar to grow into.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let log = if self.log_open { "Hide the log" } else { "Show the log" };
                    if ui.add(icon_button(Icon::Log, false)).on_hover_text(log).clicked() {
                        self.log_open = !self.log_open;
                    }
                    self.update_controls(ui, downloading);
                });
            });
            if let Update::Failed { message, page } = &self.update {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, message);
                    if let Some(page) = page {
                        ui.hyperlink_to("Release page", page);
                    }
                });
            }
            ui.separator();
            let mut pick = None;
            ui.add_enabled_ui(!downloading, |ui| {
                let browse = self.picking.is_none();
                let hint = "Choose a package .zip, or drop one on the window";
                if path_field(ui, "Package", hint, &mut self.package, browse) {
                    pick = Some(Pick::Package);
                }
                if path_field(ui, "Download to", "Choose a folder for the game's files", &mut self.output, browse) {
                    pick = Some(Pick::Output);
                }
            });
            if let Some(pick) = pick {
                self.pick(ui.ctx(), pick);
            }
            match &self.info {
                Some((_, Ok(package))) => {
                    ui.add_space(8.0);
                    show_package(ui, package, &mut self.picker, &mut self.skip, !downloading, press);
                }
                Some((_, Err(error))) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                None => welcome(ui),
            }
        });
        self.show_log(ui.ctx());
        self.show_depots(ui.ctx(), !downloading);
        let looking = self.picker.lookup.is_some();
        if self.task.is_some() || looking || matches!(self.update, Update::Checking { .. } | Update::Installing { .. }) {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// A window that opens over the middle of the main one, a margin in from
/// its edges: the log, and the depots.
fn modal<'open>(ctx: &egui::Context, title: &str) -> egui::Window<'open> {
    let room = ctx.content_rect();
    // The size is of what is inside it, so its own title and frame come off.
    let inside = (room.size() - egui::vec2(2.0 * MODAL_MARGIN + 16.0, 2.0 * MODAL_MARGIN + 44.0)).max(egui::vec2(200.0, 120.0));
    egui::Window::new(title).collapsible(false).pivot(egui::Align2::CENTER_CENTER).default_pos(room.center()).default_size(inside)
}

/// What to do first, in the middle of the space left in `ui`.
fn welcome(ui: &mut egui::Ui) {
    ui.add_space((ui.available_height() / 2.0 - 50.0).max(0.0));
    ui.vertical_centered(|ui| {
        ui.label(bold("To begin, choose a package and an output folder", 20.0));
        ui.add_space(8.0);
        let text = "The package is a .zip with a .lua file and .manifest files;\n\
                    you can also drop it onto this window. Then press Download.";
        ui.label(egui::RichText::new(text).size(15.0).weak());
    });
}

/// A row as wide as `ui`, its items lined up across it by `align`.
fn row<R>(ui: &mut egui::Ui, align: egui::Align, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), egui::Layout::left_to_right(align), add).inner
}

/// A large button, filled with the accent colour if it is the `main` one.
fn big_button(ui: &mut egui::Ui, text: &str, main: bool, enabled: bool) -> egui::Response {
    let button = if main {
        egui::Button::new(bold(text, 17.0).color(egui::Color32::WHITE)).fill(ui.visuals().selection.bg_fill)
    } else {
        egui::Button::new(egui::RichText::new(text).size(17.0))
    };
    ui.add_enabled(enabled, button.min_size(egui::vec2(140.0, 40.0)).corner_radius(8.0))
}

/// A labelled text field filling the row, with a Browse button, which can be
/// pressed if `browse`. Returns whether it was.
fn path_field(ui: &mut egui::Ui, label: &str, hint: &str, text: &mut String, browse: bool) -> bool {
    ui.horizontal(|ui| {
        row_label(ui, label);
        let button_width = 80.0;
        let field = egui::TextEdit::singleline(text).hint_text(hint);
        ui.add(field.desired_width(ui.available_width() - button_width));
        ui.add_enabled(browse, egui::Button::new("Browse…")).clicked()
    }).inner
}

/// What the window offers to choose for the package last read: from Steam's
/// record of the app once it arrives, and from the depots' files until then
/// or if it never does.
#[derive(Default)]
struct Picker {
    /// The lookup of the app's record and its DLC's names, while it runs.
    lookup: Option<Lookup>,
    record: Option<Record>,
    /// The names of DLC, as far as they are known.
    names: HashMap<u32, String>,
    sizes: BTreeMap<u32, u64>,
    tags: BTreeMap<u32, Tags>,
    offer: Offer,
    choices: Choices,
    /// What each list is searched for.
    search: HashMap<Selector, String>,
    /// Whether the depots window is open, and whether it shows every depot
    /// rather than only the chosen ones.
    list_open: bool,
    list_all: bool,
}

impl Picker {
    /// The picker for `package`, looking up its app on `mirror` if one is given.
    fn new(package: &Package, mirror: Option<&str>) -> Self {
        let sizes = package.depots.iter().map(|depot| (depot.id, depot.size())).collect();
        let mut picker = Picker { sizes, ..Picker::default() };
        // A package that carries the record needs no lookup.
        if let Some((record, names)) = appinfo::bundled(package) {
            picker.names = names;
            picker.use_record(package, Some(record));
            return picker;
        }
        picker.lookup = package.app.zip(mirror).map(|(app, mirror)| {
            log::write(format!("gui: looking up app {app}"));
            Lookup::start(mirror, app, package.depots.iter().map(|depot| depot.id).collect())
        });
        picker.use_record(package, None);
        picker
    }

    /// Go by `record`, or by the depots' files without one, and start over
    /// from its first choices.
    fn use_record(&mut self, package: &Package, record: Option<Record>) {
        self.tags = select::tags(&package.depots, record.as_ref());
        self.offer = Offer::of(&self.tags);
        self.choices = self.offer.start(Platform::here());
        self.record = record;
        log::write(format!("gui: tags of {} depots from {}: selectors {:?}, {} DLC; choosing {:?} {:?} {:?}",
                           self.tags.len(), if self.record.is_some() { "the record" } else { "the files" },
                           self.offer.selectors(), self.offer.dlc.len(),
                           self.choices.system, self.choices.arch, self.choices.language));
    }

    /// The depots the choices leave out.
    fn skip(&self) -> BTreeSet<u32> {
        select::skip(&self.tags, &self.choices)
    }

    fn dlc_name(&self, dlc: u32) -> String {
        self.names.get(&dlc).cloned().unwrap_or_else(|| format!("DLC {dlc}"))
    }

    /// Take what the lookup has found. Returns whether the record came, which
    /// changes what is chosen.
    fn collect(&mut self, package: &Package) -> bool {
        let Some(lookup) = &mut self.lookup else { return false };
        let (found, done) = (lookup.found(), lookup.is_done());
        if done {
            self.lookup = None;
        }
        let mut changed = false;
        for found in found {
            match found {
                Found::Record(Some(record)) => {
                    self.use_record(package, Some(record));
                    changed = true;
                }
                Found::Record(None) => {}
                Found::Name(dlc, name) => _ = self.names.insert(dlc, name),
            }
        }
        changed
    }
}

/// The app's name and build, and under them each kind of tag the depots
/// have. Changing one chooses the depots again by Steam's rule. With nothing
/// to choose, it says so and what to `press`.
fn show_package(ui: &mut egui::Ui, package: &Package, picker: &mut Picker, skip: &mut BTreeSet<u32>, editable: bool,
                press: Option<&str>) {
    let name = picker.record.as_ref().and_then(|record| record.name.clone())
        .or_else(|| package.app_name.clone())
        .or_else(|| package.app.map(|app| format!("App {app}")));
    // Wrapped, as a line too long for the window would push what is lined up
    // on the right out of it.
    let line = egui::Layout::left_to_right(egui::Align::Max).with_main_wrap(true);
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), line, |ui| {
        if let Some(name) = name {
            ui.label(bold(name, 28.0));
        }
        if let Some(build) = package.build_id() {
            let text = egui::RichText::new(format!("Build {build}")).size(14.0);
            match package.app {
                Some(app) => {
                    let builds = format!("https://steamdb.info/app/{app}/patchnotes/");
                    ui.hyperlink_to(text, builds).on_hover_text("The app's builds on SteamDB, to see whether there is a newer one");
                }
                None => _ = ui.label(text),
            }
        }
        if picker.lookup.is_some() {
            ui.spinner().on_hover_text("Fetching Steam's record of the app and its DLC");
        }
    });
    ui.add_space(12.0);
    let kinds = arranged(&picker.offer);
    if kinds.is_empty() {
        if let (None, Some(press)) = (&picker.lookup, press) {
            what_next(ui, press);
        }
        return;
    }
    ui.add_enabled_ui(editable, |ui| {
        if selectors(ui, picker, &kinds) {
            *skip = picker.skip();
        }
    });
}

/// What to `press`, in the middle of the space left: all there is to say
/// for a package with nothing to choose.
fn what_next(ui: &mut egui::Ui, press: &str) {
    ui.add_space(((ui.available_height() - 30.0) / 2.0).max(0.0));
    ui.vertical_centered(|ui| ui.label(bold(press, 20.0)));
}

/// What the main button does before a download, which its label says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Next {
    Checking,
    Download,
    /// The folder holds another version, which this one replaces.
    Patch,
    Resume,
    /// Everything is there already: downloading again only checks it.
    Done,
}

impl Next {
    /// `update` is whether the folder holds another version; `done` and
    /// `total` are what the last check or download found.
    pub(crate) fn of(checking: bool, update: bool, done: u64, total: u64) -> Self {
        match () {
            _ if checking => Next::Checking,
            _ if update && done < total => Next::Patch,
            _ if done > 0 && done < total => Next::Resume,
            _ if total > 0 && done >= total => Next::Done,
            _ => Next::Download,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Next::Checking => "Checking…",
            Next::Download | Next::Done => "Download",
            Next::Patch => "Patch",
            Next::Resume => "Resume",
        }
    }
}

/// What a package with nothing to choose says: what to do next, given the main
/// button and whether a `folder` is chosen. Nothing while a check or a
/// download runs, or when everything is downloaded already.
pub(crate) fn what_to_press(next: Next, folder: bool, downloading: bool) -> Option<&'static str> {
    match next {
        _ if downloading => None,
        Next::Checking | Next::Done => None,
        _ if !folder => Some("Choose a folder, then press Download."),
        Next::Download => Some("Press Download to begin."),
        Next::Patch => Some("Press Patch to begin."),
        Next::Resume => Some("Press Resume to continue."),
    }
}

/// How a kind of tag is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Look {
    /// A tile for each value, to click.
    Tiles,
    /// A boxed list of rows, with a mark for the one chosen or a box to
    /// tick for each DLC.
    List,
}

/// The kinds of tag shown and how each is drawn: tiles for a short choice,
/// a list for a long one and for DLC. Every tile kind comes first, in the
/// order of `Offer::selectors`, then every list, DLC last.
pub(crate) fn arranged(offer: &Offer) -> Vec<(Selector, Look)> {
    let look = |selector: &Selector| match selector {
        Selector::Dlc => Look::List,
        _ if offer.count(selector) >= LIST_FROM => Look::List,
        _ => Look::Tiles,
    };
    let mut kinds: Vec<(Selector, Look)> = offer.selectors().into_iter().map(|selector| {
        let look = look(&selector);
        (selector, look)
    }).collect();
    kinds.sort_by_key(|(_, look)| *look == Look::List); // Stable, and DLC is last already.
    kinds
}

/// Each kind of tag under its heading, and whether a choice changed. The
/// kinds scroll as a whole when the window is too short for them; each list
/// scrolls in its own box, which for the last takes the height left.
fn selectors(ui: &mut egui::Ui, picker: &mut Picker, kinds: &[(Selector, Look)]) -> bool {
    let room = ui.available_height(); // Inside the scroll area, there is no end to it.
    let last = kinds.iter().rposition(|(_, look)| *look == Look::List);
    let mut changed = false;
    // The bar of the whole gets room of its own beside the kinds: floating
    // over them, as the lists' own bars do, it would cover those.
    let floating = std::mem::replace(&mut ui.spacing_mut().scroll, egui::style::ScrollStyle::solid());
    egui::ScrollArea::vertical().id_salt("kinds").auto_shrink([false, true]).max_height(room).show(ui, |ui| {
        ui.spacing_mut().scroll = floating;
        let top = ui.cursor().top();
        for (index, (selector, look)) in kinds.iter().enumerate() {
            if index > 0 {
                ui.add_space(14.0);
            }
            changed |= match look {
                Look::Tiles => tiles(ui, selector, picker),
                Look::List => {
                    let left = (Some(index) == last).then(|| room - (ui.cursor().top() - top));
                    list(ui, selector, picker, left)
                }
            };
        }
    });
    changed
}

/// The heading over a kind of tag: its label in capitals, and a weak note.
fn heading(ui: &mut egui::Ui, label: &str, note: Option<&str>) {
    ui.label(egui::RichText::new(label.to_uppercase()).size(14.0).extra_letter_spacing(0.7));
    if let Some(note) = note {
        ui.label(egui::RichText::new(note).size(13.0).weak());
    }
}

/// A kind's values as tiles, up to four a line and all as wide, and whether
/// one was chosen.
fn tiles(ui: &mut egui::Ui, selector: &Selector, picker: &mut Picker) -> bool {
    row(ui, egui::Align::Center, |ui| heading(ui, selector.label(), None));
    ui.add_space(5.0);
    let values = picker.offer.values(selector, &picker.choices);
    let across = values.len().clamp(1, 4) as f32;
    let width = ((ui.available_width() - TILE_GAP * (across - 1.0)) / across).floor();
    let mut changed = false;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(TILE_GAP, TILE_GAP);
        for line in values.chunks(4) {
            ui.horizontal(|ui| {
                for value in line {
                    if ui.add(tile(&value.name, value.chosen, width)).clicked() && !value.chosen {
                        (picker.choices, changed) = (value.choices.clone(), true);
                    }
                }
            });
        }
    });
    changed
}

/// One value as a tile: lit with the accent colour when it is the one chosen.
fn tile(text: &str, chosen: bool, width: f32) -> impl egui::Widget + '_ {
    move |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(width, TILE_HEIGHT), egui::Sense::click());
        response.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, ui.is_enabled(), chosen, text));
        if ui.is_rect_visible(rect) {
            let (visuals, state) = (ui.visuals(), ui.style().interact(&response));
            let ground = visuals.widgets.noninteractive.bg_fill;
            let (fill, stroke, text) = if chosen {
                let accent = visuals.selection.bg_fill;
                (ground.lerp_to_gamma(accent, 0.25), egui::Stroke::new(1.5, accent.lerp_to_gamma(visuals.hyperlink_color, 0.5)),
                 bold(text, 16.0))
            } else {
                let lift = if response.hovered() { 0.7 } else { 0.4 };
                let stroke = if response.hovered() { state.bg_stroke } else { visuals.widgets.noninteractive.bg_stroke };
                (ground.lerp_to_gamma(visuals.widgets.inactive.weak_bg_fill, lift), stroke, egui::RichText::new(text).size(16.0))
            };
            let galley = egui::WidgetText::from(text)
                .into_galley(ui, Some(egui::TextWrapMode::Truncate), width - 24.0, egui::TextStyle::Button);
            let painter = ui.painter();
            painter.rect(rect, 8.0, fill, stroke, egui::StrokeKind::Inside);
            painter.galley(rect.center() - galley.size() / 2.0, galley, visuals.widgets.inactive.text_color());
        }
        response
    }
}

/// A kind's values, or the DLC, as a boxed list with a row for each and what
/// choosing it adds, and whether the choice changed. A long list can be
/// searched; DLC can also all be ticked or none. The box scrolls at
/// `LIST_ROWS` rows, or if `fill` is given, at the height left of it.
fn list(ui: &mut egui::Ui, selector: &Selector, picker: &mut Picker, fill: Option<f32>) -> bool {
    let start = ui.cursor().top();
    let dlc = *selector == Selector::Dlc;
    let count = picker.offer.count(selector);
    let values = picker.offer.values(selector, &picker.choices);
    // "No extra" and "None" are not counted among the values.
    let note = if dlc {
        format!("{} of {count} chosen", count - picker.choices.without.len())
    } else {
        format!("{} to choose from", values.iter().filter(|value| !value.none).count())
    };
    let mut search = picker.search.remove(selector).unwrap_or_default();
    let mut all = None;
    row(ui, egui::Align::Center, |ui| {
        heading(ui, selector.label(), Some(&note));
        if count > 8 {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if dlc {
                    if ui.button("None").clicked() {
                        all = Some(false);
                    }
                    if ui.button("All").clicked() {
                        all = Some(true);
                    }
                }
                let field = egui::TextEdit::singleline(&mut search).hint_text("Search")
                    .font(egui::FontId::proportional(14.0)).margin(egui::Margin::symmetric(10, 4));
                ui.add(field.desired_width(230.0));
            });
        }
    });
    ui.add_space(5.0);
    let wanted = search.trim().to_lowercase();
    let found = |name: &str| wanted.is_empty() || name.to_lowercase().contains(&wanted);
    let listed: Vec<(u32, String)> = if dlc {
        picker.offer.dlc.iter().map(|&dlc| (dlc, picker.dlc_name(dlc))).filter(|(_, name)| found(name)).collect()
    } else {
        Vec::new()
    };
    let values: Vec<select::Value> = values.into_iter().filter(|value| found(&value.name)).collect();
    picker.search.insert(selector.clone(), search);
    let mut changed = false;
    match all {
        Some(true) => listed.iter().for_each(|(dlc, _)| _ = picker.choices.without.remove(dlc)),
        Some(false) => picker.choices.without.extend(listed.iter().map(|(dlc, _)| dlc)),
        None => {}
    }
    changed |= all.is_some();

    let visuals = ui.visuals();
    let frame = egui::Frame::new().fill(visuals.faint_bg_color).stroke(visuals.widgets.noninteractive.bg_stroke)
        .corner_radius(8.0).inner_margin(egui::Margin::symmetric(0, 4));
    let least = LIST_ROWS * ROW_HEIGHT;
    let height = fill.map_or(least, |fill| (fill - (ui.cursor().top() - start) - frame.total_margin().sum().y).max(least));
    frame.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        // At least that tall if it has the rows for it, even where the window has
        // less room left: then the kinds scroll as a whole.
        let scroll = egui::ScrollArea::vertical().id_salt(("list", selector)).auto_shrink([false, true])
            .max_height(height).min_scrolled_height(height);
        let rows = if dlc { listed.len() } else { values.len() };
        scroll.show_rows(ui, ROW_HEIGHT, rows, |ui, range| {
            if dlc {
                for (dlc, name) in &listed[range] {
                    let ticked = !picker.choices.without.contains(dlc);
                    let size = download::human(select::dlc_size(&picker.tags, &picker.sizes, &picker.choices, *dlc));
                    if ui.add(list_row(Mark::Tick, ticked, name, Some(size))).clicked() {
                        if ticked { picker.choices.without.insert(*dlc) } else { picker.choices.without.remove(dlc) };
                        changed = true;
                    }
                }
            } else {
                for value in &values[range] {
                    let size = (!value.none)
                        .then(|| download::human(select::value_size(&picker.tags, &picker.sizes, &value.choices, selector)));
                    if ui.add(list_row(Mark::Radio, value.chosen, &value.name, size)).clicked() && !value.chosen {
                        (picker.choices, changed) = (value.choices.clone(), true);
                    }
                }
            }
        });
    });
    changed
}

/// The mark at the start of a list's row.
#[derive(Clone, Copy)]
enum Mark {
    /// Round, for one of several.
    Radio,
    /// A box to tick.
    Tick,
}

/// A row of a list: its mark, the name, and at the right what choosing it
/// adds. The whole row can be clicked, the name too.
fn list_row(mark: Mark, on: bool, name: &str, size: Option<String>) -> impl egui::Widget + '_ {
    move |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), egui::Sense::click());
        let role = match mark {
            Mark::Radio => egui::Role::RadioButton,
            Mark::Tick => egui::Role::CheckBox,
        };
        response.widget_info(|| egui::WidgetInfo::selected(role, ui.is_enabled(), on, name));
        if ui.is_rect_visible(rect) {
            let (visuals, state) = (ui.visuals(), ui.style().interact(&response));
            let painter = ui.painter();
            if response.hovered() {
                painter.rect_filled(rect, 0.0, visuals.widgets.hovered.weak_bg_fill.gamma_multiply(0.25));
            }
            let inner = rect.shrink2(egui::vec2(16.0, 0.0));
            let mark_rect = egui::Rect::from_center_size(egui::pos2(inner.left() + 8.0, rect.center().y), egui::vec2(16.0, 16.0));
            let bright = visuals.strong_text_color();
            match mark {
                Mark::Radio => {
                    painter.circle_filled(mark_rect.center(), 7.5, state.bg_fill);
                    if on {
                        painter.circle_filled(mark_rect.center(), 3.5, bright);
                    }
                }
                Mark::Tick => {
                    painter.rect_filled(mark_rect, 2.0, state.bg_fill);
                    if on {
                        let tick = mark_rect.shrink(3.5);
                        let points = vec![egui::pos2(tick.left(), tick.center().y), egui::pos2(tick.center().x - 1.0, tick.bottom()),
                                          tick.right_top()];
                        painter.add(egui::Shape::line(points, egui::Stroke::new(2.0, bright)));
                    }
                }
            }
            let text = |text: String, wrap: egui::TextWrapMode, width: f32| {
                egui::WidgetText::from(egui::RichText::new(text).size(15.0)).into_galley(ui, Some(wrap), width, egui::TextStyle::Body)
            };
            let size = size.map(|size| text(size, egui::TextWrapMode::Extend, f32::INFINITY));
            let left = mark_rect.right() + 12.0;
            let room = inner.right() - left - size.as_ref().map_or(0.0, |size| size.size().x + 16.0);
            let name = text(name.to_string(), egui::TextWrapMode::Truncate, room.max(0.0));
            let middle = |galley: &egui::Galley| rect.center().y - galley.size().y / 2.0;
            painter.galley(egui::pos2(left, middle(&name)), name, visuals.widgets.inactive.text_color());
            if let Some(size) = size {
                painter.galley(egui::pos2(inner.right() - size.size().x, middle(&size)), size, visuals.text_color());
            }
        }
        response
    }
}

/// The drawings on the top bar's buttons.
#[derive(Clone, Copy)]
enum Icon {
    /// Two arrows going round.
    Update,
    /// A terminal's prompt.
    Log,
}

/// A small button with an icon drawn on it; `lit` fills it with the accent
/// colour and adds an orange dot, for news.
fn icon_button(icon: Icon, lit: bool) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 26.0), egui::Sense::click());
        let name = match icon {
            Icon::Update => "Updates",
            Icon::Log => "Log",
        };
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), name));
        if ui.is_rect_visible(rect) {
            let (visuals, state) = (ui.visuals(), ui.style().interact(&response));
            let (fill, color) = match lit {
                true if response.hovered() => (visuals.selection.bg_fill.lerp_to_gamma(egui::Color32::WHITE, 0.1), egui::Color32::WHITE),
                true => (visuals.selection.bg_fill, egui::Color32::WHITE),
                false => (state.weak_bg_fill, state.fg_stroke.color),
            };
            let painter = ui.painter();
            painter.rect_filled(rect, 4.0, fill);
            draw_icon(painter, icon, rect.center(), egui::Stroke::new(1.5, color));
            if lit {
                let orange = egui::Color32::from_rgb(0xff, 0x8f, 0x00);
                painter.circle(rect.right_top() + egui::vec2(-3.5, 3.5), 4.5, orange, egui::Stroke::new(2.0, visuals.panel_fill));
            }
        }
        response
    }
}

/// Draw `icon` round `center`, 17 points across, from the drawing on a
/// 20 by 20 grid that the mockups have.
fn draw_icon(painter: &egui::Painter, icon: Icon, center: egui::Pos2, stroke: egui::Stroke) {
    let scale = 17.0 / 20.0;
    let at = |x: f32, y: f32| center + egui::vec2(x - 10.0, y - 10.0) * scale;
    let line = |points: &[(f32, f32)]| painter.add(egui::Shape::line(points.iter().map(|&(x, y)| at(x, y)).collect(), stroke));
    match icon {
        Icon::Update => {
            // Round the middle, clockwise, in degrees from the right.
            let arc = |from: f32, to: f32| {
                let points = (0..=16).map(|step| (from + (to - from) * step as f32 / 16.0).to_radians())
                    .map(|angle| at(10.0 + 6.5 * angle.cos(), 10.0 + 6.5 * angle.sin())).collect();
                painter.add(egui::Shape::line(points, stroke));
            };
            arc(0.0, 141.9); // From the right, under, to the lower left.
            arc(180.0, 321.9); // From the left, over, to the upper right.
            line(&[(15.6, 2.8), (15.6, 6.2), (12.2, 6.2)]);
            line(&[(4.4, 17.2), (4.4, 13.8), (7.8, 13.8)]);
        }
        Icon::Log => {
            painter.rect_stroke(egui::Rect::from_min_max(at(2.0, 3.5), at(18.0, 16.5)), 2.0 * scale, stroke, egui::StrokeKind::Middle);
            line(&[(5.5, 8.0), (8.5, 10.3), (5.5, 12.6)]);
            line(&[(10.5, 13.0), (14.5, 13.0)]);
        }
    }
}

/// The label of a path field, as wide as the other's, so that the fields line up.
fn row_label(ui: &mut egui::Ui, label: &str) {
    let width = ui.label(label).rect.width();
    ui.add_space((LABEL - width).max(0.0));
}

/// A row of the depots window: a box to tick, the ID, the tags, the size.
/// Hovering it shows the manifest, the package's comment on the depot, and
/// every tag in full, as the row may have no room for them all.
fn depot_row(ui: &mut egui::Ui, depot: &Depot, picker: &Picker, skip: &mut BTreeSet<u32>, editable: bool) {
    let words = picker.tags.get(&depot.id).map(|tags| tags.words(|dlc| picker.dlc_name(dlc))).unwrap_or_default();
    let height = ui.spacing().interact_size.y;
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    let shown = ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), height), layout, |ui| {
        let mut wanted = !skip.contains(&depot.id);
        if ui.add_enabled(editable, egui::Checkbox::without_text(&mut wanted)).changed() {
            if wanted { skip.remove(&depot.id) } else { skip.insert(depot.id) };
        }
        ui.allocate_ui_with_layout(egui::vec2(70.0, height), layout, |ui| ui.label(depot.id.to_string()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(download::human(picker.sizes.get(&depot.id).copied().unwrap_or(0)));
            ui.with_layout(layout, |ui| chips(ui, &words));
        });
    });
    let comment = depot.name.as_ref().map(|name| format!("\n{name}")).unwrap_or_default();
    let tags = if words.is_empty() { String::new() } else { format!("\n{}", words.join("\n")) };
    shown.response.on_hover_text(format!("Manifest {}{comment}{tags}", depot.manifest));
}

/// Tags in small outlined chips, or a weak "no tags". A tag too long for the
/// room left is cut short, and those there is no room for at all are counted.
fn chips(ui: &mut egui::Ui, words: &[String]) {
    if words.is_empty() {
        ui.label(egui::RichText::new("no tags").size(12.0).weak());
    }
    let outline = ui.visuals().widgets.noninteractive.bg_stroke;
    for (index, word) in words.iter().enumerate() {
        let after = words.len() - index - 1;
        // Keep room for the count, unless this is the last tag.
        let room = ui.available_width() - if after > 0 { 40.0 } else { 0.0 };
        if room < 60.0 {
            ui.label(egui::RichText::new(format!("+{}", after + 1)).size(11.0).weak());
            break;
        }
        let frame = egui::Frame::new().stroke(outline).corner_radius(9.0).inner_margin(egui::Margin::symmetric(7, 0));
        frame.show(ui, |ui| {
            ui.set_max_width(room - 16.0); // Less the chip's own margins.
            ui.add(egui::Label::new(egui::RichText::new(word).size(11.0)).truncate())
        });
    }
}

#[cfg(test)]
impl App {
    pub fn looking_up_on(mirror: &str) -> Self {
        App { mirror: mirror.into(), ..App::default() }
    }

    /// Open the package at `path`, as typing its path would.
    pub fn open(&mut self, path: &Path) {
        self.package = path.display().to_string();
        self.read_info();
    }

    /// Take what the lookup found, and say whether it is still running.
    pub fn collect_found(&mut self) -> bool {
        self.collect_lookup(false);
        self.picker.lookup.is_some()
    }

    pub fn record_name(&self) -> Option<String> {
        self.picker.record.as_ref()?.name.clone()
    }

    pub fn skip(&self) -> &BTreeSet<u32> {
        &self.skip
    }
}

/// Every piece of text the window draws for `package`, given its app's
/// `record` and the `names` of its DLC, in a window of 1000 by 700: for tests.
#[cfg(test)]
pub fn package_text(package: &Package, record: Option<Record>, names: &[(u32, &str)]) -> Vec<String> {
    Drawn::new(package, record, names, 1000.0).texts()
}

/// How far right any text reaches when `package` is shown in a window `width` wide.
#[cfg(test)]
pub fn package_right_edge(package: &Package, record: Option<Record>, width: f32) -> f32 {
    Drawn::new(package, record, &[], width).right_edge()
}

/// What the window shows of a package, drawn with no screen, which can be
/// clicked: for tests. No folder is chosen.
#[cfg(test)]
pub struct Drawn {
    ctx: egui::Context,
    package: Package,
    picker: Picker,
    skip: BTreeSet<u32>,
    width: f32,
    /// How many frames have been drawn: each is a fifth of a second after
    /// the last, so that what eases into place, as a scroll bar does, is there.
    frames: u32,
    /// The text the last frame drew, and where.
    texts: Vec<(String, egui::Rect)>,
}

#[cfg(test)]
impl Drawn {
    pub fn new(package: &Package, record: Option<Record>, names: &[(u32, &str)], width: f32) -> Self {
        let mut picker = Picker::new(package, None);
        if record.is_some() {
            picker.use_record(package, record);
        }
        picker.names.extend(names.iter().map(|(dlc, name)| (*dlc, name.to_string())));
        let ctx = egui::Context::default();
        set_fonts(&ctx);
        let mut drawn = Drawn { ctx, package: package.clone(), skip: picker.skip(), picker, width, frames: 0, texts: Vec::new() };
        for _ in 0..3 { // Some sizes take a frame to be known, and a scroll bar one more to take its room.
            drawn.frame(Vec::new());
        }
        drawn
    }

    /// Draw a frame that takes `events`.
    fn frame(&mut self, events: Vec<egui::Event>) {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(self.width, 700.0));
        let time = Some(f64::from(self.frames) * 0.2);
        self.frames += 1;
        let input = egui::RawInput { screen_rect: Some(screen), events, time, ..Default::default() };
        let press = what_to_press(Next::Download, false, false);
        let mut output = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_package(ui, &self.package, &mut self.picker, &mut self.skip, true, press);
            });
        });
        output.textures_delta.clear(); // Nothing is painted, so the font texture goes nowhere.
        self.texts = output.shapes.into_iter().filter_map(|clipped| match clipped.shape {
            egui::Shape::Text(text) => Some((text.galley.text().to_string(), text.galley.rect.translate(text.pos.to_vec2()))),
            _ => None,
        }).collect();
    }

    pub fn texts(&self) -> Vec<String> {
        self.texts.iter().map(|(text, _)| text.clone()).collect()
    }

    pub fn right_edge(&self) -> f32 {
        self.texts.iter().map(|(_, rect)| rect.right()).fold(0.0, f32::max)
    }

    /// Click the text `text` where the last frame drew it, then draw again.
    pub fn click(&mut self, text: &str) {
        let at = self.texts.iter().find(|(drawn, _)| drawn == text).map(|(_, rect)| rect.center());
        let at = at.unwrap_or_else(|| panic!("no {text:?} in {:?}", self.texts()));
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed,
                                                            modifiers: egui::Modifiers::default() };
        for events in [vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)], Vec::new()] {
            self.frame(events);
        }
    }

    pub fn choices(&self) -> &Choices {
        &self.picker.choices
    }

    pub fn skip(&self) -> &BTreeSet<u32> {
        &self.skip
    }
}
