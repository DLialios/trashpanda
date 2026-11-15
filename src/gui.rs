//! The window: choose a package and an output folder, then press Download.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

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
/// The height of the banner across the top, which is the window's title bar.
const BANNER: f32 = 52.0;
/// The banner's colours from left to right, and how far across each one is:
/// the blues of the icon's disc.
const BANNER_STOPS: [(f32, egui::Color32); 4] = [
    (0.0, egui::Color32::from_rgb(0x0b, 0x18, 0x30)),
    (0.28, egui::Color32::from_rgb(0x0d, 0x25, 0x50)),
    (0.68, egui::Color32::from_rgb(0x14, 0x4b, 0x7e)),
    (1.0, egui::Color32::from_rgb(0x13, 0x87, 0xb8)),
];
/// How far across the banner the accent colour is taken from: the blue of
/// the large buttons and of what is chosen.
const ACCENT_AT: f32 = 0.84;
/// The size of the raccoon's head on the banner.
const MARK: egui::Vec2 = egui::vec2(36.0, 30.0);
/// The width of the buttons that minimise, maximise and close the window,
/// which is what Windows gives its own.
const WINDOW_BUTTON: f32 = 46.0;
/// How near an edge of the window a press resizes it, and how near a corner
/// along that edge it resizes both ways.
const EDGE: f32 = 5.0;
const CORNER: f32 = 14.0;
/// The size of the large button at the foot of the window, unless its words
/// need it wider, and of the square one beside it.
const BIG_BUTTON: egui::Vec2 = egui::vec2(140.0, 40.0);
const ICON_BUTTON: f32 = 40.0;
/// The width of the small lines beside the big number at the foot.
const NOTE: f32 = 260.0;
/// The space between the time left and the buttons after it.
const TIME_GAP: f32 = 20.0;
/// The colour of news: the dot on a release to install.
const NEWS: egui::Color32 = egui::Color32::from_rgb(0xff, 0x8f, 0x00);
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
        // The system draws no frame: the banner is the title bar (see
        // `App::banner`), and `resize_edges` stands in for the borders.
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_icon(icon())
            .with_decorations(false)
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([640.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(TITLE, options, Box::new(|creation| {
        creation.egui_ctx.set_theme(egui::Theme::Dark);
        creation.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.visuals.selection.bg_fill = blue_at(ACCENT_AT);
        });
        set_fonts(&creation.egui_ctx);
        let mut app = App::default();
        app.check_for_update();
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

/// The raccoon's head for the banner, drawn three times the size it is
/// shown at, so that it stays sharp on a dense screen.
pub fn mark() -> egui::ColorImage {
    let png = eframe::icon_data::from_png_bytes(include_bytes!("../assets/mark.png")).expect("assets/mark.png is a PNG");
    egui::ColorImage::from_rgba_unmultiplied([png.width as usize, png.height as usize], &png.rgba)
}

/// The banner's colour `across` it, from 0 at its left to 1 at its right.
pub fn blue_at(across: f32) -> egui::Color32 {
    let next = BANNER_STOPS.iter().position(|&(at, _)| at >= across).unwrap_or(BANNER_STOPS.len() - 1).max(1);
    let ((from, before), (to, after)) = (BANNER_STOPS[next - 1], BANNER_STOPS[next]);
    before.lerp_to_gamma(after, ((across - from) / (to - from)).clamp(0.0, 1.0))
}

/// The banner's blues from left to right, one pixel high: what the banner
/// is filled with.
fn blues() -> egui::ColorImage {
    let width = 256;
    egui::ColorImage::new([width, 1], (0..width).map(|x| blue_at(x as f32 / (width - 1) as f32)).collect())
}

/// What is chosen: the package path and output folder as typed, which
/// reading of the package, and the depots left out.
#[derive(Clone, Default, PartialEq)]
struct Choice {
    package: String,
    output: String,
    /// Counts the times a package was read, so that a package read again,
    /// at the same path, is checked again: it may hold other manifests.
    read: u64,
    skip: BTreeSet<u32>,
}

/// The package last read for display.
struct Info {
    /// Its path as typed.
    path: String,
    /// Its size and modification time when it was read, if it had them.
    stamp: Option<(u64, Option<SystemTime>)>,
    package: Result<Arc<Package>, String>,
}

/// The size and modification time of the file at `path`, which change when
/// another file takes its place.
fn stamp(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    std::fs::metadata(path).ok().map(|meta| (meta.len(), meta.modified().ok()))
}

pub struct App {
    package: String,
    output: String,
    /// Depots not to download.
    skip: BTreeSet<u32>,
    /// The package last read for display, and what reading it gave.
    info: Option<Info>,
    /// The `Choice::read` of `info`.
    reads: u64,
    /// Set to read the package again, though its path is unchanged: it was
    /// chosen or dropped on the window again.
    reread: bool,
    /// Set to read the package again if its size or modification time
    /// changed, as it does when a newer package replaces it under the same
    /// name: when the window comes back to the front, and before a download.
    restat: bool,
    /// Whether the window was in front at the last frame.
    focused: bool,
    picker: Picker,
    /// Where Steam's records of apps come from.
    mirror: String,
    /// The choice whose state the readout shows.
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
    /// The raccoon's head on the banner, once it has been drawn.
    mark: Option<egui::TextureHandle>,
    /// The banner's blues, once it has been filled with them.
    blues: Option<egui::TextureHandle>,
    /// What last went wrong with a check, a download or saving the log, in
    /// a few words; the log has the details.
    error: Option<String>,
}

/// Checking for a newer version, and installing it. The check is made once,
/// when the window opens, and shows only a newer version.
enum Update {
    /// Not checked yet, or the check found nothing newer, or failed.
    Idle,
    Checking(JoinHandle<anyhow::Result<Option<Release>>>),
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
            reads: 0,
            reread: false,
            restat: false,
            focused: false,
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
            mark: None,
            blues: None,
            error: None,
        }
    }
}

impl App {
    fn choice(&self) -> Choice {
        Choice { package: self.package.trim().into(), output: self.output.trim().into(), read: self.reads,
                 skip: self.skip.clone() }
    }

    fn parsed(&self) -> Option<Arc<Package>> {
        match &self.info {
            Some(Info { package: Ok(package), .. }) => Some(package.clone()),
            _ => None,
        }
    }

    /// The package with only the depots ticked, if any are.
    fn selected(&self) -> Option<Arc<Package>> {
        let package = self.parsed()?;
        let package = if self.skip.is_empty() { package } else { Arc::new(package.select(|id| !self.skip.contains(&id))) };
        Some(package).filter(|package| !package.depots.is_empty())
    }

    /// Read the package again whenever its path changes, or `reread` or
    /// `restat` asks for it, to show what is in it. Returns whether it was
    /// read. While a download runs, the package it is downloading stays.
    fn read_info(&mut self, downloading: bool) -> bool {
        if downloading {
            return false; // `reread` and `restat` wait for it to end.
        }
        let (reread, restat) = (std::mem::take(&mut self.reread), std::mem::take(&mut self.restat));
        let path = self.package.trim();
        if path.is_empty() {
            self.info = None;
            self.picker = Picker::default();
            return false;
        }
        let replaced = |info: &Info| restat && stamp(Path::new(path)) != info.stamp;
        if self.info.as_ref().is_none_or(|info| info.path != path || reread || replaced(info)) {
            log::write(format!("gui: package {path:?}"));
            // Before it is read, so that a change while it is read is seen next time.
            let stamp = stamp(Path::new(path));
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
            self.info = Some(Info { path: path.to_string(), stamp, package });
            // What was found on disk for the package before is not used for this one.
            self.reads += 1;
            return true;
        }
        false
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
        let now = self.choice();
        let same = |checked: &Choice| (&checked.package, &checked.output, checked.read) == (&now.package, &now.output, now.read);
        self.on_disk.as_ref().filter(|(checked, _)| same(checked)).map(|(_, on_disk)| on_disk.clone())
    }

    fn start(&mut self) {
        // If another package has taken this one's place, show it instead.
        self.restat = true;
        if self.read_info(false) {
            return;
        }
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
                                               (the log button at the bottom right, then Export log…)."));
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
            Pick::Package => (self.package, self.reread) = (path.display().to_string(), true),
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

    fn check_for_update(&mut self) {
        log::write("gui: checking for updates");
        self.update = Update::Checking(thread::spawn(|| Updater::new().check()));
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
            // Nobody asked for the check, so only the log hears that it failed.
            Update::Checking(job) if job.is_finished() => match job.join() {
                Ok(Ok(Some(release))) => Update::Available(release),
                Ok(Ok(None)) => Update::Idle,
                Ok(Err(error)) => {
                    log::write(format!("gui: update check failed: {error:#}"));
                    Update::Idle
                }
                Err(_) => {
                    log::write("gui: the update check crashed");
                    Update::Idle
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

    /// The banner across the top, which is the window's title bar too: the
    /// raccoon, the name and the version, news of a release, and the buttons
    /// that minimise, maximise and close the window. Dragging it moves the
    /// window, and a double click maximises it or puts it back.
    fn banner(&mut self, ui: &mut egui::Ui, downloading: bool) {
        let ctx = ui.ctx().clone();
        let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
        // Before what is on it, so that the buttons on it take their own presses.
        let (rect, bar) = ui.allocate_exact_size(egui::vec2(ui.available_width(), BANNER), egui::Sense::click_and_drag());
        if bar.double_clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        } else if bar.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let blues = self.blues.get_or_insert_with(|| ctx.load_texture("blues", blues(), egui::TextureOptions::LINEAR)).id();
        let mark = self.mark.get_or_insert_with(|| {
            // It is drawn smaller than it is, which only a mipmap does smoothly.
            let options = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
            ctx.load_texture("mark", mark(), options)
        });
        let painter = ui.painter();
        let (white, middle) = (egui::Color32::WHITE, rect.center().y);
        let whole = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        painter.image(blues, rect, whole, white);
        let mark_rect = egui::Rect::from_min_size(egui::pos2(rect.left() + 15.0, middle - MARK.y / 2.0), MARK);
        painter.image(mark.id(), mark_rect, whole, white);
        let title = painter.layout_no_wrap(TITLE.into(), egui::FontId::new(25.0, egui::FontFamily::Name(BOLD.into())), white);
        let version = concat!("v", env!("CARGO_PKG_VERSION")).to_string();
        let version = painter.layout_no_wrap(version, egui::FontId::proportional(13.0), white.gamma_multiply(0.6));
        // The version stands on the line the name is written on.
        let title_at = egui::pos2(mark_rect.right() + 9.0, middle - title.size().y / 2.0);
        let version_at = egui::pos2(title_at.x + title.size().x + 9.0, title_at.y + ascent(&title) - ascent(&version));
        let buttons = rect.right() - 3.0 * WINDOW_BUTTON;
        let news = egui::Rect::from_min_max(egui::pos2(version_at.x + version.size().x + 12.0, rect.top()),
                                            egui::pos2(buttons - 8.0, rect.bottom()));
        painter.galley(title_at, title, white);
        painter.galley(version_at, version, white);
        let across = |rect: egui::Rect, layout: egui::Layout| egui::UiBuilder::new().max_rect(rect).layout(layout);
        ui.scope_builder(across(news, egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.set_clip_rect(news.intersect(ui.clip_rect()));
            self.release_news(ui, downloading);
        });
        let buttons = egui::Rect::from_min_max(egui::pos2(buttons, rect.top()), rect.max);
        ui.scope_builder(across(buttons, egui::Layout::right_to_left(egui::Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if ui.add(window_button(Icon::Close)).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if ui.add(window_button(if maximized { Icon::Restore } else { Icon::Maximize })).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if ui.add(window_button(Icon::Minimize)).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        });
    }

    /// On the banner, beside the version: a newer version, in a pill that
    /// installs it when pressed, and then says how that is going.
    fn release_news(&mut self, ui: &mut egui::Ui, downloading: bool) {
        match &self.update {
            Update::Available(release) => {
                // Why it cannot be installed now.
                let blocked = if !update::REPLACES_ITSELF {
                    Some("A debug build is never replaced by an update")
                } else if downloading {
                    Some("Pause the download first; Resume continues it afterwards")
                } else {
                    None
                };
                let pill = ui.add_enabled(blocked.is_none(), release_pill(format!("Update to {}", release.version)));
                let pressed = pill.on_hover_text(format!("Install version {}", release.version))
                    .on_disabled_hover_text(blocked.unwrap_or_default()).clicked();
                if pressed {
                    let release = release.clone();
                    self.install_update(release);
                }
            }
            // The pill stays while it does, with nothing left to press.
            Update::Installing { version, .. } => {
                ui.add_enabled(false, release_pill(format!("Updating to {version}…")));
            }
            Update::Restarting => {
                ui.add_enabled(false, release_pill("Restarting…".into()));
            }
            Update::Idle | Update::Checking(_) | Update::Failed { .. } => {}
        }
    }

    /// The button that shows or hides the log.
    fn log_button(&mut self, ui: &mut egui::Ui) {
        let hint = if self.log_open { "Hide the log" } else { "Show the log" };
        if ui.add(icon_button(Icon::Log)).on_hover_text(hint).clicked() {
            self.log_open = !self.log_open;
        }
    }

    /// The progress in big type with the Pause button, or before a download,
    /// how much there is to download with the Download, Resume or Patch button.
    /// The log's button is beside either, and both are one row of the same
    /// height, so that the foot of the window stays where it is.
    fn readout(&mut self, ui: &mut egui::Ui, downloading: bool, checking: bool, updating: bool) {
        let (done, total) = (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed));
        let percent = format!("{:.1}%", self.progress.fraction() * 100.0);
        let big = |text: String| bold(text, 64.0);
        // Beside the foot of the number, from the bottom up: the space is
        // about what the big digits leave under their baseline.
        let foot = egui::Layout::bottom_up(egui::Align::Min);
        if downloading {
            let rates = self.meter.update(&self.progress, Instant::now());
            // Before it downloads, it copies what an older version has in other places.
            let copying = self.progress.local.load(Relaxed);
            row(ui, egui::Align::Center, |ui| {
                let number = ui.label(big(percent)).rect;
                // The time left is in large type before the buttons, if all of it fits there.
                let large = ui.painter().layout_no_wrap(rates.left_text(), egui::FontId::new(40.0, egui::FontFamily::Name(BOLD.into())),
                                                        ui.visuals().strong_text_color());
                let word = ui.painter().layout_no_wrap("left".into(), egui::FontId::proportional(22.0), ui.visuals().weak_text_color());
                let time = egui::vec2(large.size().x + 8.0 + word.size().x, large.size().y);
                let others = NOTE + TIME_GAP + ICON_BUTTON + BIG_BUTTON.x + 3.0 * ui.spacing().item_spacing.x;
                let roomy = ui.available_width() >= others + time.x;
                let pace = match (copying, roomy) {
                    (true, _) => "using what is already here".into(),
                    (false, true) => rates.speed_text(),
                    (false, false) => format!("{}   ·   {} left", rates.speed_text(), rates.left_text()),
                };
                ui.allocate_ui_with_layout(egui::vec2(NOTE, number.height()), foot, |ui| {
                    ui.add_space(12.0);
                    let amount = format!("{} of {}", download::human(done), download::human(total));
                    ui.label(egui::RichText::new(format!("{amount}\n{pace}")).size(16.0).weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pausing = self.progress.stop.load(Relaxed);
                    if big_button(ui, if pausing { "Pausing…" } else { "Pause" }, !pausing).clicked() {
                        log::write("gui: pause");
                        self.progress.stop.store(true, Relaxed);
                    }
                    self.log_button(ui);
                    if roomy {
                        // Level with the buttons, and "left" on the line the figures stand on.
                        ui.add_space(TIME_GAP);
                        let (rect, _) = ui.allocate_exact_size(time, egui::Sense::hover());
                        let word_at = rect.left_top() + egui::vec2(large.size().x + 8.0, ascent(&large) - ascent(&word));
                        ui.painter().galley(rect.left_top(), large, egui::Color32::PLACEHOLDER);
                        ui.painter().galley(word_at, word, egui::Color32::PLACEHOLDER);
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
            ui.allocate_ui_with_layout(egui::vec2(NOTE, number.height()), foot, |ui| {
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
                if big_button(ui, label, ready).clicked() {
                    self.start();
                }
                self.log_button(ui);
            });
        });
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
            (self.package, self.reread) = (path.display().to_string(), true);
        }
        // The package may have been replaced while the window was behind others.
        let focused = ui.input(|input| input.focused);
        self.restat |= focused && !self.focused;
        self.focused = focused;
        self.read_info(downloading);
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
        egui::Panel::top("banner").frame(egui::Frame::NONE).exact_size(BANNER).show_separator_line(false)
            .show(ui, |ui| self.banner(ui, downloading));
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            if let Update::Failed { message, page } = &self.update {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, message);
                    if let Some(page) = page {
                        ui.hyperlink_to("Release page", page);
                    }
                });
            }
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
                Some(Info { package: Ok(package), .. }) => {
                    ui.add_space(8.0);
                    show_package(ui, package, &mut self.picker, &mut self.skip, !downloading, press);
                }
                Some(Info { package: Err(error), .. }) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                None => welcome(ui),
            }
        });
        self.show_log(ui.ctx());
        self.show_depots(ui.ctx(), !downloading);
        let looking = self.picker.lookup.is_some();
        if self.task.is_some() || looking || matches!(self.update, Update::Checking(_) | Update::Installing { .. }) {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        resize_edges(ui.ctx()); // Last, so that the pointer it shows over an edge stays.
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

/// How far under its top the line that `text` is written on lies.
fn ascent(text: &egui::Galley) -> f32 {
    text.rows.first().and_then(|row| row.row.glyphs.first()).map_or(0.0, |glyph| glyph.font_ascent)
}

/// A row as wide as `ui`, its items lined up across it by `align`.
fn row<R>(ui: &mut egui::Ui, align: egui::Align, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), egui::Layout::left_to_right(align), add).inner
}

/// A large button in the accent colour: the one thing to press next.
fn big_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(bold(text, 17.0).color(egui::Color32::WHITE)).fill(ui.visuals().selection.bg_fill);
    ui.add_enabled(enabled, button.min_size(BIG_BUTTON).corner_radius(8.0))
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

/// The drawings on the buttons that have no words.
#[derive(Clone, Copy)]
enum Icon {
    /// A terminal's prompt.
    Log,
    Minimize,
    Maximize,
    /// Two windows, one behind the other: back to the size before.
    Restore,
    Close,
}

impl Icon {
    /// What the button is for, for a screen reader and for a pointer resting on it.
    fn name(self) -> &'static str {
        match self {
            Icon::Log => "Log",
            Icon::Minimize => "Minimize",
            Icon::Maximize => "Maximize",
            Icon::Restore => "Restore",
            Icon::Close => "Close",
        }
    }
}

/// A square button with an icon drawn on it, as tall as the big button it
/// is beside and as round at the corners.
fn icon_button(icon: Icon) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(ICON_BUTTON), egui::Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), icon.name()));
        if ui.is_rect_visible(rect) {
            let state = ui.style().interact(&response);
            ui.painter().rect_filled(rect, 8.0, state.weak_bg_fill);
            draw_icon(ui.painter(), icon, rect.center(), 20.0, egui::Stroke::new(1.5, state.fg_stroke.color));
        }
        response
    }
}

/// A button of the title bar, as tall as the banner: it lights up under the
/// pointer, and the one that closes the window does so in red, as on Windows.
fn window_button(icon: Icon) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(egui::vec2(WINDOW_BUTTON, BANNER), egui::Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), icon.name()));
        if ui.is_rect_visible(rect) {
            let white = egui::Color32::WHITE;
            let fill = match (icon, response.hovered()) {
                (_, false) => egui::Color32::TRANSPARENT,
                (Icon::Close, true) => egui::Color32::from_rgb(0xc4, 0x2b, 0x1c),
                (_, true) => white.gamma_multiply(0.14),
            };
            ui.painter().rect_filled(rect, 0.0, fill);
            draw_icon(ui.painter(), icon, rect.center(), 12.5, egui::Stroke::new(1.0, white));
        }
        response.on_hover_text(icon.name())
    }
}

/// News of a release, on the banner: a dot in the colour of news and the
/// words, in a pill to press.
fn release_pill(words: String) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        let white = egui::Color32::WHITE;
        let text = ui.painter().layout_no_wrap(words.clone(), egui::FontId::proportional(13.0), white);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(text.size().x + 35.0, 24.0), egui::Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), &words));
        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            painter.rect_filled(rect, rect.height() / 2.0, white.gamma_multiply(if response.hovered() { 0.28 } else { 0.16 }));
            painter.circle_filled(egui::pos2(rect.left() + 13.0, rect.center().y), 4.0, NEWS);
            painter.galley(egui::pos2(rect.left() + 23.0, rect.center().y - text.size().y / 2.0), text, white);
        }
        response
    }
}

/// Draw `icon` round `center`, `size` points across, from its drawing on a
/// 20 by 20 grid. The window's buttons fill the grid from 2 to 18.
fn draw_icon(painter: &egui::Painter, icon: Icon, center: egui::Pos2, size: f32, stroke: egui::Stroke) {
    let scale = size / 20.0;
    let at = |x: f32, y: f32| center + egui::vec2(x - 10.0, y - 10.0) * scale;
    let line = |points: &[(f32, f32)]| painter.add(egui::Shape::line(points.iter().map(|&(x, y)| at(x, y)).collect(), stroke));
    let square = |from: (f32, f32), to: (f32, f32)| {
        painter.rect_stroke(egui::Rect::from_min_max(at(from.0, from.1), at(to.0, to.1)), 2.0 * scale, stroke, egui::StrokeKind::Middle)
    };
    match icon {
        Icon::Log => {
            square((2.0, 3.5), (18.0, 16.5));
            line(&[(5.5, 8.0), (8.5, 10.3), (5.5, 12.6)]);
            line(&[(10.5, 13.0), (14.5, 13.0)]);
        }
        Icon::Minimize => {
            line(&[(2.0, 10.0), (18.0, 10.0)]);
        }
        Icon::Maximize => {
            square((2.0, 2.0), (18.0, 18.0));
        }
        Icon::Restore => {
            square((2.0, 5.5), (14.5, 18.0));
            line(&[(5.5, 5.5), (5.5, 2.0), (18.0, 2.0), (18.0, 14.5), (14.5, 14.5)]);
        }
        Icon::Close => {
            line(&[(2.0, 2.0), (18.0, 18.0)]);
            line(&[(18.0, 2.0), (2.0, 18.0)]);
        }
    }
}

/// Which way a press at `at` resizes a window that fills `room`: none
/// unless it is within `EDGE` of an edge, and both ways within `CORNER` of
/// a corner along it.
pub fn edge_at(room: egui::Rect, at: egui::Pos2) -> Option<egui::ResizeDirection> {
    use egui::ResizeDirection::*;
    let (west, east) = (at.x - room.left(), room.right() - at.x);
    let (north, south) = (at.y - room.top(), room.bottom() - at.y);
    if west.min(east).min(north).min(south) >= EDGE {
        return None;
    }
    Some(match (north < CORNER, south < CORNER, west < CORNER, east < CORNER) {
        (true, _, true, _) => NorthWest,
        (true, _, _, true) => NorthEast,
        (_, true, true, _) => SouthWest,
        (_, true, _, true) => SouthEast,
        (true, ..) => North,
        (_, true, ..) => South,
        (_, _, true, _) => West,
        _ => East,
    })
}

/// The borders the system would have drawn: a strip along each edge of the
/// window, where the pointer shows which way a press resizes it. They lie
/// over everything else, so that a press there reaches nothing underneath.
/// A window that fills the screen has none.
fn resize_edges(ctx: &egui::Context) {
    let filling = |viewport: &egui::ViewportInfo| viewport.maximized.unwrap_or(false) || viewport.fullscreen.unwrap_or(false);
    if ctx.input(|input| filling(input.viewport())) {
        return;
    }
    let room = ctx.content_rect();
    let strips = [
        ("north", room.with_max_y(room.top() + EDGE)),
        ("south", room.with_min_y(room.bottom() - EDGE)),
        ("west", room.with_max_x(room.left() + EDGE)),
        ("east", room.with_min_x(room.right() - EDGE)),
    ];
    for (name, strip) in strips {
        let area = egui::Area::new(egui::Id::unique(("edge", name))).order(egui::Order::Foreground).fixed_pos(strip.min);
        area.show(ctx, |ui| {
            let (_, response) = ui.allocate_exact_size(strip.size(), egui::Sense::click_and_drag());
            let Some(direction) = response.hover_pos().and_then(|at| edge_at(room, at)) else { return };
            use egui::{CursorIcon as Pointer, ResizeDirection as To};
            ctx.set_cursor_icon(match direction {
                To::North => Pointer::ResizeNorth,
                To::South => Pointer::ResizeSouth,
                To::East => Pointer::ResizeEast,
                To::West => Pointer::ResizeWest,
                To::NorthEast => Pointer::ResizeNorthEast,
                To::SouthEast => Pointer::ResizeSouthEast,
                To::NorthWest => Pointer::ResizeNorthWest,
                To::SouthWest => Pointer::ResizeSouthWest,
            });
            if ui.input(|input| input.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
            }
        });
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
        self.read_info(false);
    }

    /// Choose the package at `path` in the file dialog, or drop it on the
    /// window, as the next frame takes it.
    pub fn choose(&mut self, path: &Path) {
        (self.package, self.reread) = (path.display().to_string(), true);
    }

    /// Bring the window back to the front, as the next frame sees it.
    pub fn come_back(&mut self) {
        self.restat = true;
    }

    pub fn set_output(&mut self, path: &Path) {
        self.output = path.display().to_string();
    }

    /// Do what a frame does before it draws anything, until no check runs,
    /// nor a download that was paused. A download not paused keeps running.
    pub fn settle(&mut self) {
        let started = Instant::now();
        loop {
            self.collect();
            let downloading = self.task.as_ref().is_some_and(|task| task.download);
            self.read_info(downloading);
            self.collect_lookup(downloading);
            self.check();
            if self.task.as_ref().is_none_or(|task| task.download && !self.progress.stop.load(Relaxed)) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "the check never ended");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Start a download that fetches nothing and runs until it is paused.
    pub fn start_idle_download(&mut self) {
        self.spawn(true, |progress| {
            while !progress.stop.load(Relaxed) {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None)
        });
    }

    pub fn pause(&self) {
        self.progress.stop.store(true, Relaxed);
    }

    /// The depots, and their files, that Download would download.
    pub fn chosen_files(&self) -> Vec<(u32, String)> {
        let package = self.selected().into_iter();
        package.flat_map(|package| package.depots.iter()
                .flat_map(|depot| depot.entries.iter().map(|entry| (depot.id, entry.name.clone()))).collect::<Vec<_>>())
            .collect()
    }

    /// What the readout says is there, and how much the depots chosen hold.
    pub fn done_and_total(&self) -> (u64, u64) {
        (self.progress.done.load(Relaxed), self.progress.total.load(Relaxed))
    }

    /// Whether a check would start from what was found on disk before.
    pub fn has_previous(&self) -> bool {
        self.previous().is_some()
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
