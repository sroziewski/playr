//! The YouTube window: search YouTube, or open a channel (a link or `@name`) and pick one of
//! its newest videos.
//!
//! yt-dlp takes several seconds per page, so every request runs on its own thread with its
//! progress shown, and a newer request cancels the one still running. Thumbnails come in on a
//! few threads of their own and appear as they arrive. What is shown and waited for is kept in
//! [`browse::Nav`]; this module only draws it and runs the threads.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Key, RichText, Sense, Stroke, vec2};

use crate::browse::{self, Arrival, Back, Channel, Item, Listing, Nav, Query, Recent, Request};
use crate::media::format_time;

const THUMB: egui::Vec2 = egui::Vec2::new(176.0, 99.0);
const THUMB_WORKERS: usize = 4;
/// After the window closes on a pick, clicks on the stage under it are ignored this long, so
/// the second click of a double click does not pause the video just started.
const CLOSE_GUARD: Duration = Duration::from_millis(500);

struct Job {
    id: u64,
    abort: Arc<AtomicBool>,
    progress: Arc<Mutex<String>>,
    rx: Receiver<Result<Listing, String>>,
}

enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

/// Thumbnails waiting to be fetched and the workers fetching them, under one lock: a worker
/// that finds the queue empty leaves in the same breath, so an id queued just then always has
/// a worker coming for it.
#[derive(Default)]
struct ThumbQueue {
    ids: VecDeque<String>,
    workers: usize,
}

type Fetched = (String, Result<browse::Rgb, String>);

pub struct Browser {
    pub open: bool,
    input: String,
    in_channel: String,
    nav: Nav,
    job: Option<Job>,
    recent: Recent,
    thumbs: HashMap<String, Thumb>,
    thumb_queue: Arc<Mutex<ThumbQueue>>,
    thumb_tx: Sender<Fetched>,
    thumb_rx: Receiver<Fetched>,
    focus_input: bool,
    scroll_top: bool,
    closed_at: Option<Instant>,
}

impl Default for Browser {
    fn default() -> Self {
        let (thumb_tx, thumb_rx) = channel();
        Self {
            open: false,
            input: String::new(),
            in_channel: String::new(),
            nav: Nav::default(),
            job: None,
            recent: browse::recent_path().map(|p| Recent::load(&p)).unwrap_or_default(),
            thumbs: HashMap::new(),
            thumb_queue: Arc::new(Mutex::new(ThumbQueue::default())),
            thumb_tx,
            thumb_rx,
            focus_input: false,
            scroll_top: false,
            closed_at: None,
        }
    }
}

impl Browser {
    /// Opens the window with the search field ready for typing.
    pub fn show_window(&mut self) {
        self.open = true;
        self.focus_input = true;
    }

    /// Opens the window on the newest videos of the channel at `url`.
    pub fn open_channel(&mut self, ctx: &egui::Context, url: String) {
        self.open = true;
        self.input = url.clone();
        self.run(ctx, Query::Channel { url }, Back::Clear);
    }

    /// Whether clicks on the stage should still be ignored after the window closed.
    pub fn just_closed(&self) -> bool {
        self.closed_at.is_some_and(|t| t.elapsed() < CLOSE_GUARD)
    }

    fn close(&mut self) {
        self.open = false;
        self.closed_at = Some(Instant::now());
        self.cancel();
    }

    fn run(&mut self, ctx: &egui::Context, query: Query, back: Back) {
        if !matches!(query, Query::InChannel { .. }) {
            self.in_channel.clear();
        }
        let request = self.nav.ask(query, false, back);
        self.start(ctx, request);
    }

    fn start(&mut self, ctx: &egui::Context, request: Request) {
        self.stop_job();
        let abort = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(match &request.query {
            _ if request.append => "Loading more videos…".to_string(),
            Query::Search { text } => format!("Searching YouTube for \u{201c}{text}\u{201d}…"),
            Query::Channel { .. } => "Opening the channel…".to_string(),
            Query::InChannel { text, .. } => format!("Searching the channel for \u{201c}{text}\u{201d}…"),
        }));
        let (tx, rx) = channel();
        let (a, p, repaint) = (abort.clone(), progress.clone(), ctx.clone());
        let (query, have) = (request.query.clone(), request.have);
        std::thread::spawn(move || {
            let say = |m: &str| *p.lock().unwrap() = m.to_string();
            let result = browse::fetch(&query, have, &a, &say);
            if !a.load(Ordering::Relaxed) {
                let _ = tx.send(result);
                repaint.request_repaint();
            }
        });
        self.job = Some(Job { id: request.id, abort, progress, rx });
    }

    fn stop_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort.store(true, Ordering::Relaxed);
        }
    }

    fn cancel(&mut self) {
        self.stop_job();
        self.nav.cancel();
    }

    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the search stopped unexpectedly".into()),
        };
        let id = job.id;
        self.job = None;
        let channel = result.as_ref().ok().and_then(|l| l.channel.clone());
        if self.nav.arrived(id, result) == Arrival::New {
            if let Some(channel) = channel {
                self.remember(&channel);
            }
            self.scroll_top = true;
            self.prune_thumbs();
        }
    }

    /// Drops the thumbnails (and queued fetches) of lists that can no longer be on screen.
    fn prune_thumbs(&mut self) {
        let keep = self.nav.shown_ids();
        self.thumbs.retain(|id, _| keep.contains(id));
        self.thumb_queue.lock().unwrap().ids.retain(|id| keep.contains(id));
    }

    fn remember(&mut self, channel: &Channel) {
        self.recent.record(channel);
        if let Some(path) = browse::recent_path() {
            let _ = self.recent.save(&path);
        }
    }

    fn forget(&mut self, url: &str) {
        self.recent.remove(url);
        if let Some(path) = browse::recent_path() {
            let _ = self.recent.save(&path);
        }
    }

    fn poll_thumbs(&mut self, ctx: &egui::Context) {
        while let Ok((id, result)) = self.thumb_rx.try_recv() {
            // A thumbnail of a list pruned meanwhile is not wanted any more.
            if !self.thumbs.contains_key(&id) {
                continue;
            }
            let thumb = match result {
                Ok((w, h, rgb)) => {
                    let image = egui::ColorImage::from_rgb([w, h], &rgb);
                    Thumb::Ready(ctx.load_texture(format!("yt-thumb-{id}"), image, egui::TextureOptions::LINEAR))
                }
                Err(_) => Thumb::Failed,
            };
            self.thumbs.insert(id, thumb);
        }
    }

    fn want_thumb(&mut self, ctx: &egui::Context, id: &str) {
        if self.thumbs.contains_key(id) {
            return;
        }
        self.thumbs.insert(id.to_string(), Thumb::Loading);
        {
            let mut queue = self.thumb_queue.lock().unwrap();
            queue.ids.push_back(id.to_string());
            if queue.workers >= THUMB_WORKERS {
                return;
            }
            queue.workers += 1;
        }
        let (queue, tx, repaint) = (self.thumb_queue.clone(), self.thumb_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            loop {
                let next = {
                    let mut q = queue.lock().unwrap();
                    let next = q.ids.pop_front();
                    if next.is_none() {
                        q.workers -= 1;
                    }
                    next
                };
                let Some(id) = next else { return };
                // A decoder panic on a damaged picture must not take the worker's place with it.
                let result = std::panic::catch_unwind(|| browse::fetch_thumbnail(&browse::thumbnail_url(&id)))
                    .unwrap_or_else(|_| Err("unreadable picture".into()));
                if tx.send((id, result)).is_err() {
                    queue.lock().unwrap().workers -= 1;
                    return;
                }
                repaint.request_repaint();
            }
        });
    }

    /// Draws the window. Returns the address of the video picked to play.
    pub fn show(&mut self, ctx: &egui::Context, playing_id: Option<&str>) -> Option<String> {
        if !self.open {
            return None;
        }
        self.poll();
        self.poll_thumbs(ctx);
        let mut picked = None;
        let mut open = true;
        let size = ctx.content_rect().size();
        egui::Window::new("YouTube")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .default_size(vec2(760.0f32.min(size.x - 40.0), 620.0f32.min(size.y - 60.0)))
            .max_width(size.x - 16.0)
            .max_height(size.y - 16.0)
            .show(ctx, |ui| picked = self.contents(ui, playing_id));
        if !open || picked.is_some() || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.close();
        }
        picked
    }

    fn contents(&mut self, ui: &mut egui::Ui, playing_id: Option<&str>) -> Option<String> {
        let ctx = ui.ctx().clone();
        // Right to left, so the field takes what the button leaves: a field sized from the
        // window's width would widen a resizable window a little on every frame.
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let search = ui.add_enabled(!self.input.trim().is_empty(), egui::Button::new("Search"));
                let edit = egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Search YouTube, or paste a channel link or @name")
                    .desired_width(ui.available_width());
                let response = ui.add(edit);
                if std::mem::take(&mut self.focus_input) {
                    response.request_focus();
                }
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (search.clicked() || enter)
                    && let Some(query) = browse::query_for(&self.input)
                {
                    self.run(&ctx, query, Back::Clear);
                }
            });
        });
        ui.add_space(4.0);

        let mut picked = None;
        let mut action = None;
        self.header(ui, &mut action);
        ui.separator();

        let loading_new = self.nav.pending().is_some_and(|r| !r.append);
        if loading_new {
            self.progress_row(ui, &mut action);
        }
        if let Some(failed) = self.nav.error.as_ref().filter(|f| !f.append) {
            Self::error_row(ui, &failed.message, &mut action);
        }
        let Some(view) = &self.nav.view else {
            if !loading_new && self.nav.error.is_none() {
                self.start_page(ui, &mut action);
            }
            self.act(&ctx, action);
            return None;
        };
        if view.listing.items.is_empty() && !loading_new {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.weak("No videos found"));
        }
        let ids: Vec<String> = view.listing.items.iter().map(|i| i.id.clone()).collect();
        for id in &ids {
            self.want_thumb(&ctx, id);
        }
        let Some(view) = &self.nav.view else { return None };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let in_channel = matches!(view.query, Query::Channel { .. } | Query::InChannel { .. });
        scroll.show(ui, |ui| {
            for item in &view.listing.items {
                let current = playing_id == Some(item.id.as_str());
                match self.row(ui, item, current, now, !in_channel) {
                    RowClick::Play => picked = Some(item.url()),
                    RowClick::Channel(c) => action = Some(Action::Channel(c)),
                    RowClick::None => {}
                }
            }
            ui.add_space(6.0);
            if self.nav.pending().is_some_and(|r| r.append) {
                self.progress_row(ui, &mut action);
            } else if let Some(failed) = self.nav.error.as_ref().filter(|f| f.append) {
                Self::error_row(ui, &failed.message, &mut action);
            } else if self.nav.can_show_more() && !loading_new {
                ui.vertical_centered(|ui| {
                    if ui.button(format!("Show {} more", browse::PAGE)).clicked() {
                        action = Some(Action::More);
                    }
                });
            }
            ui.add_space(6.0);
        });
        self.act(&ctx, action);
        picked
    }

    fn header(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let has_back = self.nav.back.is_some();
        let Some(view) = &self.nav.view else { return };
        match &view.query {
            Query::Search { text } => {
                ui.label(RichText::new(format!("Results for \u{201c}{text}\u{201d}")).weak());
            }
            Query::Channel { url } | Query::InChannel { url, .. } => {
                let name = view.listing.channel.as_ref().map_or_else(|| url.clone(), |c| c.name.clone());
                let url = url.clone();
                ui.horizontal(|ui| {
                    if has_back && ui.button("Back to results").clicked() {
                        *action = Some(Action::Back);
                    }
                    ui.label(RichText::new(name).heading().strong());
                    let what = match &view.query {
                        Query::InChannel { text, .. } => format!("videos matching \u{201c}{text}\u{201d}"),
                        _ => "newest videos".to_string(),
                    };
                    ui.label(RichText::new(what).weak());
                });
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.in_channel)
                        .hint_text("Search this channel…")
                        .desired_width(260.0);
                    let response = ui.add(edit);
                    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.add_enabled(!self.in_channel.trim().is_empty(), egui::Button::new("Search")).clicked() || enter {
                        *action = Some(match self.in_channel.trim() {
                            "" => Action::Run(Query::Channel { url: url.clone() }),
                            text => Action::Run(Query::InChannel { url: url.clone(), text: text.to_string() }),
                        });
                    }
                    if matches!(view.query, Query::InChannel { .. }) && ui.button("Newest videos").clicked() {
                        *action = Some(Action::Run(Query::Channel { url: url.clone() }));
                    }
                });
            }
        }
    }

    fn start_page(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.add_space(8.0);
        if !self.recent.channels.is_empty() {
            ui.label(RichText::new("Recent channels").strong());
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                for channel in &self.recent.channels {
                    let chip = ui.add(egui::Button::new(channel.name.as_str()).corner_radius(12.0));
                    if chip.clicked() {
                        *action = Some(Action::Recent(channel.clone()));
                    }
                    chip.context_menu(|ui| {
                        if ui.button("Forget this channel").clicked() {
                            *action = Some(Action::Forget(channel.url.clone()));
                            ui.close();
                        }
                    });
                    chip.on_hover_text(format!("{}\nRight-click to forget", channel.url));
                }
            });
            ui.add_space(12.0);
        }
        ui.weak("Type what to look for and press Enter, or paste a channel link (youtube.com/@name) or an @name to see its newest videos.");
        ui.add_space(4.0);
        ui.weak("In a channel's list you can search its videos and load more. Click a video to play it.");
    }

    fn progress_row(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        let message = self.job.as_ref().map(|j| j.progress.lock().unwrap().clone()).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(message);
            if ui.small_button("Cancel").clicked() {
                *action = Some(Action::Cancel);
            }
        });
    }

    fn error_row(ui: &mut egui::Ui, error: &str, action: &mut Option<Action>) {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(ui.visuals().error_fg_color, error);
            if ui.button("Try again").clicked() {
                *action = Some(Action::Retry);
            }
        });
    }

    fn act(&mut self, ctx: &egui::Context, action: Option<Action>) {
        match action {
            None => {}
            Some(Action::Run(query)) => self.run(ctx, query, Back::Keep),
            Some(Action::Channel(channel)) => self.run(ctx, Query::Channel { url: channel.url }, Back::FromResults),
            Some(Action::Recent(channel)) => {
                self.input = channel.url.clone();
                self.run(ctx, Query::Channel { url: channel.url }, Back::Clear);
            }
            Some(Action::More) => {
                if let Some(request) = self.nav.more() {
                    self.start(ctx, request);
                }
            }
            Some(Action::Retry) => {
                if let Some(request) = self.nav.retry() {
                    self.start(ctx, request);
                }
            }
            Some(Action::Cancel) => self.cancel(),
            Some(Action::Back) => {
                self.stop_job();
                if self.nav.go_back() {
                    self.scroll_top = true;
                }
            }
            Some(Action::Forget(url)) => self.forget(&url),
        }
    }

    fn row(&self, ui: &mut egui::Ui, item: &Item, current: bool, now: i64, show_channel: bool) -> RowClick {
        let mut click = RowClick::None;
        let background = ui.painter().add(egui::Shape::Noop);
        let inner = ui.scope_builder(egui::UiBuilder::new().sense(Sense::click()), |ui| {
            ui.set_width(ui.available_width());
            egui::Frame::new().inner_margin(6.0).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    self.thumbnail(ui, item);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        let title = if current { format!("Playing: {}", item.title) } else { item.title.clone() };
                        ui.add(egui::Label::new(RichText::new(title).strong().size(15.0)).wrap().selectable(false));
                        ui.add_space(2.0);
                        if let Some(channel) = item.channel.as_ref().filter(|_| show_channel) {
                            let link = ui
                                .add(egui::Label::new(RichText::new(&channel.name).weak()).sense(Sense::click()).truncate())
                                .on_hover_text("Show this channel's newest videos")
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if link.hovered() {
                                ui.painter().hline(link.rect.x_range(), link.rect.bottom(), Stroke::new(1.0, ui.visuals().weak_text_color()));
                            }
                            if link.clicked() {
                                click = RowClick::Channel(channel.clone());
                            }
                        }
                        let meta: Vec<String> = [item.views.map(browse::views_label), item.uploaded.map(|t| browse::age_label(now, t))]
                            .into_iter()
                            .flatten()
                            .collect();
                        if !meta.is_empty() {
                            ui.add(egui::Label::new(RichText::new(meta.join(" · ")).weak()).selectable(false));
                        }
                        if item.upcoming {
                            ui.add(egui::Label::new(RichText::new("Upcoming: not started yet").italics().weak()).selectable(false));
                        }
                    });
                });
            });
        });
        let response = inner.response;
        let visuals = ui.visuals();
        let fill = if current {
            visuals.selection.bg_fill.gamma_multiply(0.35)
        } else if response.hovered() && !item.upcoming {
            visuals.widgets.hovered.weak_bg_fill
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().set(background, egui::Shape::rect_filled(response.rect, CornerRadius::same(6), fill));
        let response = if item.upcoming {
            response.on_hover_text("This video has not started yet")
        } else {
            response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Play \u{201c}{}\u{201d}", item.title))
        };
        if response.clicked() && !item.upcoming && matches!(click, RowClick::None) {
            click = RowClick::Play;
        }
        click
    }

    fn thumbnail(&self, ui: &mut egui::Ui, item: &Item) {
        let (rect, _) = ui.allocate_exact_size(THUMB, Sense::hover());
        let painter = ui.painter_at(rect);
        let radius = CornerRadius::same(6);
        match self.thumbs.get(&item.id) {
            Some(Thumb::Ready(texture)) => {
                egui::Image::new((texture.id(), THUMB)).corner_radius(radius).paint_at(ui, rect);
            }
            Some(Thumb::Failed) => {
                painter.rect_filled(rect, radius, ui.visuals().faint_bg_color);
                painter.text(rect.center(), Align2::CENTER_CENTER, "YouTube", FontId::proportional(14.0), ui.visuals().weak_text_color());
            }
            _ => {
                painter.rect_filled(rect, radius, ui.visuals().faint_bg_color);
                painter.rect_stroke(rect, radius, Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Inside);
            }
        }
        let badge = if item.live {
            Some(("LIVE".to_string(), Color32::from_rgb(204, 0, 0)))
        } else if item.upcoming {
            Some(("SOON".to_string(), Color32::from_black_alpha(200)))
        } else {
            item.duration.map(|d| (format_time(d), Color32::from_black_alpha(200)))
        };
        if let Some((text, fill)) = badge {
            let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
            let size = galley.size() + vec2(8.0, 2.0);
            let at = rect.right_bottom() - size - vec2(4.0, 4.0);
            let badge_rect = egui::Rect::from_min_size(at, size);
            painter.rect_filled(badge_rect, CornerRadius::same(3), fill);
            painter.galley(badge_rect.min + vec2(4.0, 1.0), galley, Color32::WHITE);
        }
    }
}

enum Action {
    /// Another list of the same channel: the way back stays.
    Run(Query),
    /// A channel picked in search results.
    Channel(Channel),
    Recent(Channel),
    More,
    Retry,
    Cancel,
    Back,
    Forget(String),
}

enum RowClick {
    None,
    Play,
    Channel(Channel),
}
