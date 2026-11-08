//! The window: choose a package and an output folder, then press Download.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::depot::{self, Package};
use crate::download::{self, Meter, OnDisk, Progress};
use crate::log;
use crate::update::{self, Release, Unwritable, Updater, Version};

const TITLE: &str = "Trash Panda";

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([800.0, 640.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(TITLE, options, Box::new(|creation| {
        creation.egui_ctx.set_theme(egui::Theme::Dark);
        let mut app = App::default();
        app.check_for_update(true);
        Ok(Box::new(app))
    }))
}

/// What is chosen: the package path and output folder as typed, and the
/// depots left out.
#[derive(Clone, Default, PartialEq)]
struct Choice {
    package: String,
    output: String,
    skip: BTreeSet<u32>,
}

struct App {
    package: String,
    output: String,
    jobs: usize,
    /// Depots not to download.
    skip: BTreeSet<u32>,
    /// The package path last read for display, and what reading it gave.
    info: Option<(String, Result<Arc<Package>, String>)>,
    /// The choice whose state the progress bar shows.
    shown: Choice,
    /// What is on disk for a choice, from the last check or download of it.
    on_disk: Option<(Choice, Arc<OnDisk>)>,
    progress: Arc<Progress>,
    meter: Meter,
    task: Option<Task>,
    update: Update,
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
            jobs: crate::DEFAULT_JOBS,
            skip: BTreeSet::new(),
            info: None,
            shown: Choice::default(),
            on_disk: None,
            progress: Arc::default(),
            meter: Meter::default(),
            task: None,
            update: Update::Idle,
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
        } else if self.info.as_ref().is_none_or(|(read, _)| read != path) {
            log::write(format!("gui: package {path:?}"));
            let package = depot::read_package(Path::new(path)).map(Arc::new).map_err(|error| {
                log::write(format!("gui: cannot use the package: {error:#}"));
                log::brief(&error)
            });
            self.info = Some((path.to_string(), package));
            self.skip.clear();
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
        let (output, jobs, previous) = (PathBuf::from(self.output.trim()), self.jobs, self.previous());
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
                                               (Show log, then Export log…)."));
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

    /// The update button and what it found, laid out right to left.
    fn update_controls(&mut self, ui: &mut egui::Ui, downloading: bool) {
        match &mut self.update {
            Update::Checking { quiet: false, .. } => {
                ui.label("Checking for updates…");
                ui.spinner();
            }
            Update::Available(release) => {
                let release = release.clone();
                let button = ui.add_enabled(!downloading, egui::Button::new("Update"));
                if button.on_disabled_hover_text("Pause the download first; Resume continues it afterwards").clicked() {
                    self.install_update(release.clone());
                }
                if downloading {
                    ui.weak("Pause to update");
                }
                ui.hyperlink_to(format!("Version {} is available", release.version), &release.page)
                    .on_hover_text("What is new in it");
            }
            Update::Installing { version, .. } => {
                ui.label(format!("Updating to {version}…"));
                ui.spinner();
            }
            Update::Restarting => {
                ui.label("Restarting…");
            }
            Update::Idle | Update::Current | Update::Failed { .. } | Update::Checking { quiet: true, .. } => {
                if ui.button("Check for updates").clicked() {
                    match &mut self.update {
                        Update::Checking { quiet, .. } => *quiet = false, // Show the quiet check instead.
                        _ => self.check_for_update(false),
                    }
                }
                if matches!(self.update, Update::Current) {
                    ui.weak("Up to date");
                }
            }
        }
    }

    /// The progress in big type with the Pause button, or before a download,
    /// how much there is to download with the Download or Resume button.
    fn readout(&mut self, ui: &mut egui::Ui, downloading: bool, checking: bool, updating: bool) {
        let (done, total) = (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed));
        let fraction = self.progress.fraction();
        let percent = format!("{:.1}%", fraction * 100.0);
        let big = |text: String| egui::RichText::new(text).size(64.0).strong();
        let bar = |ui: &mut egui::Ui| ui.add(egui::ProgressBar::new(fraction).desired_height(14.0).corner_radius(7.0));
        if downloading {
            let rates = self.meter.update(&self.progress, Instant::now());
            row(ui, egui::Align::Max, |ui| {
                ui.label(big(percent));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                    ui.label(egui::RichText::new("left").size(22.0).weak());
                    ui.label(egui::RichText::new(rates.left_text()).size(40.0).strong());
                });
            });
            ui.add_space(6.0);
            bar(ui);
            ui.add_space(8.0);
            row(ui, egui::Align::Center, |ui| {
                let (done, total) = (download::human(done), download::human(total));
                ui.label(egui::RichText::new(format!("{done} of {total}   ·   {}", rates.speed_text())).size(16.0).weak());
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
        let (amount, note) = match size {
            _ if started => (percent, format!("downloaded\n{} of {}", download::human(done), download::human(total))),
            Some(size) => (download::human(size), "to download".into()),
            None => (String::new(), String::new()),
        };
        row(ui, egui::Align::Center, |ui| {
            ui.label(big(amount));
            let note = if checking { "checking what is\nalready downloaded…".into() } else { note };
            ui.label(egui::RichText::new(note).size(16.0).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match () {
                    _ if checking => "Checking…",
                    _ if done > 0 && done < total => "Resume",
                    _ => "Download",
                };
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
        egui::Window::new("Log")
            .open(&mut self.log_open)
            .collapsible(false)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .default_pos(ctx.content_rect().center_bottom() - egui::vec2(0.0, 40.0))
            .default_size([700.0, 260.0])
            .show(ctx, |ui| {
                let export = ui.button("Export log…").on_hover_text("Save the log to a text file, for a bug report");
                if export.clicked()
                    && let Err(error) = export_log()
                {
                    log::write(format!("gui: {error:#}"));
                    self.error = Some(log::brief(&error));
                }
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
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect();
        self.collect_update(ui.ctx());
        let downloading = self.task.as_ref().is_some_and(|task| task.download);
        let updating = matches!(self.update, Update::Installing { .. } | Update::Restarting);
        let dropped = ui.input(|input| input.raw.dropped_files.first().map(|file| file.path().to_path_buf()));
        if let (false, Some(path)) = (downloading, dropped) {
            log::write(format!("gui: file dropped: {path:?}"));
            self.package = path.display().to_string();
        }
        self.read_info();
        self.check();
        let checking = self.task.as_ref().is_some_and(|task| !task.download);
        egui::Panel::bottom("footer").show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = if self.log_open { "Hide log" } else { "Show log" };
                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    self.log_open = !self.log_open;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!downloading, |ui| {
                        let jobs = ui.add(egui::DragValue::new(&mut self.jobs).range(1..=64));
                        jobs.on_disabled_hover_text("Pause to change");
                        ui.label("Parallel downloads");
                    });
                });
            });
        });
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
                ui.label(egui::RichText::new(TITLE).size(30.0).italics().strong());
                ui.label(egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
            ui.add_enabled_ui(!downloading, |ui| {
                let hint = "Choose a package .zip, or drop one on the window";
                path_field(ui, "Package", hint, &mut self.package, || {
                    rfd::FileDialog::new().add_filter("Package", &["zip"]).pick_file()
                });
                let start = self.output.trim().to_string();
                path_field(ui, "Output folder", "Choose a folder for the game's files", &mut self.output, || {
                    let dialog = rfd::FileDialog::new();
                    if start.is_empty() { dialog } else { dialog.set_directory(start) }.pick_folder()
                });
            });
            match &self.info {
                Some((_, Ok(package))) => {
                    ui.add_space(8.0);
                    show_package(ui, package, &mut self.skip, !downloading);
                }
                Some((_, Err(error))) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                None => welcome(ui),
            }
        });
        self.show_log(ui.ctx());
        if self.task.is_some() || matches!(self.update, Update::Checking { .. } | Update::Installing { .. }) {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// What to do first, in the middle of the space left in `ui`.
fn welcome(ui: &mut egui::Ui) {
    ui.add_space((ui.available_height() / 2.0 - 50.0).max(0.0));
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new("To begin, choose a package and an output folder").size(20.0).strong());
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
    let text = egui::RichText::new(text).size(17.0);
    let button = if main {
        egui::Button::new(text.strong().color(egui::Color32::WHITE)).fill(ui.visuals().selection.bg_fill)
    } else {
        egui::Button::new(text)
    };
    ui.add_enabled(enabled, button.min_size(egui::vec2(140.0, 40.0)).corner_radius(8.0))
}

/// A labelled text field filling the row, with a Browse button that runs `pick`.
fn path_field(ui: &mut egui::Ui, label: &str, hint: &str, text: &mut String, pick: impl FnOnce() -> Option<PathBuf>) {
    ui.horizontal(|ui| {
        let label_width = ui.label(label).rect.width();
        ui.add_space((96.0 - label_width).max(0.0));
        let button_width = 80.0;
        let field = egui::TextEdit::singleline(text).hint_text(hint);
        ui.add(field.desired_width(ui.available_width() - button_width));
        if ui.button("Browse…").clicked()
            && let Some(path) = pick()
        {
            *text = path.display().to_string();
        }
    });
}

/// Save the session log to a file the user picks.
fn export_log() -> anyhow::Result<()> {
    let Some(path) = rfd::FileDialog::new().set_file_name("trashpanda-log.txt").save_file() else { return Ok(()) };
    log::write(format!("gui: saving the log to {path:?}"));
    log::save(&path).context("could not save the log")
}

/// The app and the depots in the chosen package, with a box to tick for each
/// depot to download. The heading row stays put while the depots scroll.
fn show_package(ui: &mut egui::Ui, package: &Package, skip: &mut BTreeSet<u32>, editable: bool) {
    let mut app = Vec::new();
    if let Some(name) = &package.app_name {
        app.push(egui::RichText::new(name).strong());
    }
    if let Some(id) = package.app {
        app.push(format!("App {id}").into());
    }
    if let Some(build) = &package.build {
        app.push(format!("Build {build}").into());
    }
    if !app.is_empty() {
        ui.horizontal(|ui| {
            for (index, part) in app.into_iter().enumerate() {
                if index > 0 {
                    ui.separator();
                }
                ui.label(part);
            }
        });
    }
    let height = ui.spacing().interact_size.y;
    TableBuilder::new(ui)
        .id_salt("depots")
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center)) // Keeps labels on one line.
        .columns(Column::auto(), 6)
        .column(Column::remainder())
        .header(height, |mut header| {
            header.col(|ui| {
                let mut all = skip.is_empty();
                let toggle = ui.add_enabled(editable, egui::Checkbox::without_text(&mut all));
                if toggle.on_hover_text("Tick or untick every depot").changed() {
                    *skip = if all { BTreeSet::new() } else { package.depots.iter().map(|depot| depot.id).collect() };
                }
            });
            for heading in ["Depot", "Manifest", "Date", "Files", "Size", ""] {
                header.col(|ui| _ = ui.strong(heading));
            }
        })
        .body(|mut body| {
            for depot in &package.depots {
                body.row(height, |mut row| {
                    row.col(|ui| {
                        let mut wanted = !skip.contains(&depot.id);
                        if ui.add_enabled(editable, egui::Checkbox::without_text(&mut wanted)).changed() {
                            if wanted {
                                skip.remove(&depot.id);
                            } else {
                                skip.insert(depot.id);
                            }
                        }
                    });
                    row.col(|ui| _ = ui.label(depot.id.to_string()).on_hover_ui(|ui| largest_files(ui, depot)));
                    row.col(|ui| _ = ui.label(depot.manifest.to_string()));
                    row.col(|ui| _ = ui.label(depot.created.map(download::date).unwrap_or_default()));
                    row.col(|ui| _ = ui.label(depot.files().to_string()));
                    row.col(|ui| _ = ui.label(download::human(depot.size())));
                    let name = depot.name.as_deref().unwrap_or_default();
                    row.col(|ui| _ = ui.label(name).on_hover_ui(|ui| largest_files(ui, depot)));
                });
            }
            if package.depots.len() > 1 {
                let chosen: Vec<_> = package.depots.iter().filter(|depot| !skip.contains(&depot.id)).collect();
                body.row(height, |mut row| {
                    row.col(|_| {});
                    row.col(|ui| _ = ui.strong(if skip.is_empty() { "Total" } else { "Ticked" }));
                    row.col(|_| {});
                    row.col(|_| {});
                    row.col(|ui| _ = ui.strong(chosen.iter().map(|depot| depot.files()).sum::<usize>().to_string()));
                    row.col(|ui| _ = ui.strong(download::human(chosen.iter().map(|depot| depot.size()).sum())));
                    row.col(|_| {});
                });
            }
        });
}

/// A depot's biggest files, which often show what it is for.
fn largest_files(ui: &mut egui::Ui, depot: &depot::Depot) {
    let mut files: Vec<_> = depot.entries.iter().filter(|entry| !entry.is_dir()).collect();
    files.sort_by_key(|entry| std::cmp::Reverse(entry.size));
    let mut text = format!("Depot {}, largest files:", depot.id);
    for entry in files.iter().take(8) {
        _ = write!(text, "\n{:>10}  {}", download::human(entry.size), entry.name);
    }
    ui.label(egui::RichText::new(text).monospace());
}
