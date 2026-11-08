//! The window: choose a package and an output folder, then press Download.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::depot::{self, Package};
use crate::download::{self, Meter, OnDisk, Progress};

const TITLE: &str = "Trash Panda";

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([800.0, 640.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(TITLE, options, Box::new(|_| Ok(Box::new(App::default()))))
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
}

/// A check or a download running on its own thread.
struct Task {
    download: bool,
    choice: Choice,
    handle: JoinHandle<Option<OnDisk>>,
}

impl Default for App {
    fn default() -> Self {
        let output = std::env::current_dir().map(|dir| dir.display().to_string()).unwrap_or_default();
        Self {
            package: String::new(),
            output,
            jobs: crate::DEFAULT_JOBS,
            skip: BTreeSet::new(),
            info: None,
            shown: Choice::default(),
            on_disk: None,
            progress: Arc::default(),
            meter: Meter::default(),
            task: None,
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
            let package = depot::read_package(Path::new(path)).map(Arc::new).map_err(|error| format!("{error:#}"));
            self.info = Some((path.to_string(), package));
            self.skip.clear();
        }
    }

    /// Run `job` on a thread and show its progress.
    fn spawn(&mut self, download: bool, job: impl FnOnce(&Progress) -> Option<OnDisk> + Send + 'static) {
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
        self.spawn(true, move |progress| match download::run(&package, &output, jobs, previous.as_deref(), progress) {
            Ok(summary) => summary.on_disk,
            Err(error) => {
                progress.log(format!("Error: {error:#}"));
                None
            }
        });
    }

    /// When the package, folder or depots change, find out how much is already
    /// downloaded there.
    fn check(&mut self) {
        if self.choice() == self.shown {
            return;
        }
        if self.task.is_some() {
            self.progress.stop.store(true, Relaxed); // Checking an earlier choice; check again once it stops.
            return;
        }
        let output = PathBuf::from(self.output.trim());
        let (Some(package), true) = (self.selected(), output.is_dir()) else {
            self.progress = Arc::default();
            self.shown = self.choice();
            return;
        };
        let previous = self.previous();
        self.spawn(false, move |progress| {
            download::check(&package, &output, previous.as_deref(), progress).unwrap_or_else(|error| {
                progress.log(format!("Error: {error:#}"));
                None
            })
        });
    }

    /// Keep what a finished task found on disk.
    fn collect(&mut self) {
        if self.task.as_ref().is_some_and(|task| task.handle.is_finished()) {
            let task = self.task.take().unwrap();
            if let Ok(Some(on_disk)) = task.handle.join() {
                self.on_disk = Some((task.choice, Arc::new(on_disk)));
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect();
        let downloading = self.task.as_ref().is_some_and(|task| task.download);
        let dropped = ui.input(|input| input.raw.dropped_files.first().map(|file| file.path().to_path_buf()));
        if let (false, Some(path)) = (downloading, dropped) {
            self.package = path.display().to_string();
        }
        self.read_info();
        self.check();
        let checking = self.task.as_ref().is_some_and(|task| !task.download);
        egui::CentralPanel::default().show(ui, |ui| {
            let row = egui::Layout::left_to_right(egui::Align::Max);
            ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), row, |ui| {
                ui.label(egui::RichText::new(TITLE).size(30.0).italics().strong());
                ui.label(egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).weak());
            });
            ui.separator();
            ui.add_enabled_ui(!downloading, |ui| {
                ui.label("Package (.zip with a .lua and .manifest files; you can drop it on the window)");
                path_field(ui, &mut self.package, || {
                    rfd::FileDialog::new().add_filter("Package", &["zip"]).pick_file()
                });
                ui.label("Output folder");
                let start = self.output.clone();
                path_field(ui, &mut self.output, || rfd::FileDialog::new().set_directory(start).pick_folder());
            });
            match &self.info {
                Some((_, Ok(package))) => {
                    ui.add_space(8.0);
                    show_package(ui, package, &mut self.skip, !downloading);
                }
                Some((_, Err(error))) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                None => {}
            }
            ui.add_space(8.0);
            let (done, total) = (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed));
            ui.horizontal(|ui| {
                if downloading {
                    let pausing = self.progress.stop.load(Relaxed);
                    let label = if pausing { "Pausing…" } else { "Pause" };
                    if ui.add_enabled(!pausing, egui::Button::new(label)).clicked() {
                        self.progress.stop.store(true, Relaxed);
                    }
                } else {
                    let label = match () {
                        _ if checking => "Checking…",
                        _ if done > 0 && done < total => "Resume",
                        _ => "Download",
                    };
                    let wanted = |package: Arc<Package>| package.depots.iter().any(|depot| !self.skip.contains(&depot.id));
                    let ready = !checking && self.parsed().is_some_and(wanted);
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                        self.start();
                    }
                }
                ui.add_enabled_ui(!downloading, |ui| {
                    ui.label("Parallel downloads");
                    ui.add(egui::DragValue::new(&mut self.jobs).range(1..=64));
                });
                if downloading {
                    ui.weak("Pause to change");
                }
            });
            ui.add_space(8.0);
            let fraction = self.progress.fraction();
            ui.add(egui::ProgressBar::new(fraction).text(format!("{:.1}%", fraction * 100.0)));
            if total > 0 {
                ui.horizontal(|ui| {
                    if downloading {
                        let rates = self.meter.update(&self.progress, Instant::now());
                        ui.label("Speed");
                        ui.strong(rates.speed_text());
                        ui.separator();
                        ui.label("Time left");
                        ui.strong(rates.left_text());
                        ui.separator();
                    } else if checking {
                        ui.label("Checking what is already downloaded");
                        ui.separator();
                    }
                    ui.label(format!("{} of {} downloaded", download::human(done), download::human(total)));
                });
            }
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                for line in self.progress.lines(0) {
                    ui.label(line);
                }
            });
        });
        if self.task.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// A text field filling the row, with a Browse button that runs `pick`.
fn path_field(ui: &mut egui::Ui, text: &mut String, pick: impl FnOnce() -> Option<PathBuf>) {
    ui.horizontal(|ui| {
        let button_width = 80.0;
        ui.add(egui::TextEdit::singleline(text).desired_width(ui.available_width() - button_width));
        if ui.button("Browse…").clicked()
            && let Some(path) = pick()
        {
            *text = path.display().to_string();
        }
    });
}

/// The app and the depots in the chosen package, with a box to tick for each
/// depot to download.
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
    egui::ScrollArea::vertical().id_salt("depots").max_height(220.0).show(ui, |ui| {
        egui::Grid::new("depots").striped(true).show(ui, |ui| {
            let mut all = skip.is_empty();
            let toggle = ui.add_enabled(editable, egui::Checkbox::without_text(&mut all));
            if toggle.on_hover_text("Tick or untick every depot").changed() {
                *skip = if all { BTreeSet::new() } else { package.depots.iter().map(|depot| depot.id).collect() };
            }
            for heading in ["Depot", "Manifest", "Date", "Files", "Size", ""] {
                ui.strong(heading);
            }
            ui.end_row();
            for depot in &package.depots {
                let mut wanted = !skip.contains(&depot.id);
                if ui.add_enabled(editable, egui::Checkbox::without_text(&mut wanted)).changed() {
                    if wanted {
                        skip.remove(&depot.id);
                    } else {
                        skip.insert(depot.id);
                    }
                }
                ui.label(depot.id.to_string()).on_hover_ui(|ui| largest_files(ui, depot));
                ui.label(depot.manifest.to_string());
                ui.label(depot.created.map(download::date).unwrap_or_default());
                ui.label(depot.files().to_string());
                ui.label(download::human(depot.size()));
                ui.label(depot.name.as_deref().unwrap_or_default()).on_hover_ui(|ui| largest_files(ui, depot));
                ui.end_row();
            }
            if package.depots.len() > 1 {
                let chosen: Vec<_> = package.depots.iter().filter(|depot| !skip.contains(&depot.id)).collect();
                ui.label("");
                ui.strong(if skip.is_empty() { "Total" } else { "Ticked" });
                ui.label("");
                ui.label("");
                ui.strong(chosen.iter().map(|depot| depot.files()).sum::<usize>().to_string());
                ui.strong(download::human(chosen.iter().map(|depot| depot.size()).sum()));
                ui.end_row();
            }
        });
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
