//! Native projection of the overview: one borderless, non-activating panel
//! per display above everything, holding a Core Animation layer tree.
//!
//! ```text
//! panel
//! └─ root
//!    ├─ backdrop                   the wallpaper, blurred (opaque throughout)
//!    ├─ tint                       opacity 0 → 1
//!    └─ container                  vertical scroll
//!       ├─ per row: viewport       the row's "screen": wallpaper, shrinks
//!       │           bar            menu bar, fades out
//!       │           pan            horizontal pan of the row's columns
//!       │           └─ tiled cards
//!       └─ floating cards          above every row, not panned
//! ```
//!
//! Every animated layer keeps fixed bounds and moves by `position` plus
//! `transform.scale`, which the compositor applies without re-rasterising
//! (animating `bounds` on rounded, masked, image-backed layers forces a
//! redraw per frame and drops frames). Discrete transitions are
//! `CASpringAnimation`s on the render server; gesture scrubbing sets the
//! model values directly with implicit actions disabled.
//!
//! Window contents start as static snapshots from `CGWindowListCreateImage`
//! (instant, cached between opens); `ScreenCaptureKit` streams then replace
//! them with the live windows (see `live.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSPanel, NSScreen, NSScreenSaverWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction, kCAGravityResize, kCAGravityResizeAspectFill};
use tracing::{debug, warn};

use super::capture::{
    BACKDROP_BLUR, Landed, SendImage, Shot, app_icon, desktop_picture, spawn_shots,
};
pub use super::capture::{WindowServerInfo, window_server_info};
use super::layers::{
    Animated, CARD_RADIUS, FADE_KEY, add_tile, color, fade_out_at_end, row_label, set_contents,
};
use super::live::{Live, Wanted};
use super::{DisplayLayout, Rect, Spring};
use crate::events::{Event, EventSender};
use crate::platform::Pid;

/// Where a capture's image goes once it lands.
struct Target {
    layers: Vec<Retained<CALayer>>,
    /// Drop the placeholder colour (cards) once the image is in.
    clear_background: bool,
    /// Needed before the panels are shown (the active row's windows, the
    /// wallpaper, the menu bar): without them the overlay wouldn't match
    /// the real screen it covers.
    gate: bool,
    /// Key in [`OverviewManager::cache`], and the log name.
    key: String,
    /// The window a card shows; a late snapshot doesn't cover it once its
    /// live stream is running.
    window_id: Option<i32>,
}

/// How long the panels wait for gating captures before showing anyway.
const REVEAL_TIMEOUT: Duration = Duration::from_millis(250);

// ── Click / key / scroll forwarding view ────────────────────────────────

struct OverviewViewIvars {
    sender: EventSender,
    /// Display origin in absolute CG coordinates.
    origin: (f64, f64),
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruOverviewView"]
    #[ivars = OverviewViewIvars]
    struct OverviewView;

    impl OverviewView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.cg_point(event);
            _ = self.ivars().sender.send(Event::OverviewClick { point });
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            match event.keyCode() {
                53 => _ = self.ivars().sender.send(Event::OverviewDismiss),
                36 => _ = self.ivars().sender.send(Event::OverviewAccept),
                _ => {}
            }
        }
    }
);

impl OverviewView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        sender: EventSender,
        origin: (f64, f64),
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(OverviewViewIvars { sender, origin });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn cg_point(&self, event: &NSEvent) -> CGPoint {
        let local = event.locationInWindow();
        let height = self.bounds().size.height;
        let (ox, oy) = self.ivars().origin;
        CGPoint::new(ox + local.x, oy + (height - local.y))
    }
}

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruOverviewPanel"]
    struct OverviewPanel;

    impl OverviewPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
    }
);

// ── Panels ──────────────────────────────────────────────────────────────

struct Card {
    animated: Animated,
    /// The window's snapshot.
    content: Retained<CALayer>,
    window_id: i32,
    /// On the active row: captured at full resolution, before showing.
    best: bool,
    /// Capture order (lower first), see [`Rank`].
    rank: Rank,
}

impl Card {
    fn shot(&self) -> Shot {
        Shot::Window {
            id: self.window_id,
            best: self.best,
        }
    }
}

/// Capture order, most important first. The focused window and the ones
/// next to it are what the eye is on as the overview opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Focused,
    Neighbour,
    /// A floating window on the active row: few, on top, and in view.
    Floating,
    Screen,
    Row,
    Backdrop,
    Hidden,
}

impl Rank {
    /// A card's rank from its row and its distance (in tiled columns) to
    /// the focused window; `None` when the row has no focused window.
    fn of_card(active: bool, focused: bool, floating: bool, distance: Option<usize>) -> Self {
        match (active, focused, distance) {
            (false, ..) => Self::Hidden,
            (true, true, _) | (true, _, Some(0)) => Self::Focused,
            (true, _, Some(1)) => Self::Neighbour,
            _ if floating => Self::Floating,
            _ => Self::Row,
        }
    }

    /// Captured fresh before showing even when a cached image exists.
    fn wants_fresh(self) -> bool {
        matches!(self, Self::Focused | Self::Neighbour)
    }
}

struct RowLayers {
    viewport: Animated,
    /// The menu bar inside the row's screen: there at the start so the
    /// overlay matches the real screen, gone once open.
    bar: Option<Animated>,
    pan: Animated,
    /// The workspace's name above the row, shown once open.
    label: Animated,
}

struct Panel {
    window: Retained<OverviewPanel>,
    window_number: i32,
    /// Parent of every layer below; faded out as a close lands.
    root: Retained<CALayer>,
    backdrop: Animated,
    tint: Animated,
    container: Animated,
    rows: Vec<RowLayers>,
    cards: Vec<Card>,
    /// The display's backing scale (pixels per point).
    scale: f64,
}

impl Panel {
    fn layers(&self) -> impl Iterator<Item = &Animated> {
        [&self.backdrop, &self.tint, &self.container]
            .into_iter()
            .chain(self.rows.iter().flat_map(|row| {
                [
                    Some(&row.viewport),
                    row.bar.as_ref(),
                    Some(&row.pan),
                    Some(&row.label),
                ]
                .into_iter()
                .flatten()
            }))
            .chain(self.cards.iter().map(|card| &card.animated))
    }
}

pub struct OverviewManager {
    mtm: MainThreadMarker,
    sender: EventSender,
    panels: Vec<Panel>,
    generation: u64,
    /// Identifies the current build's captures; results of older builds
    /// are dropped.
    batch: u64,
    targets: Vec<Option<Target>>,
    /// Captures of this batch not landed yet, and how many of them gate.
    outstanding: usize,
    gates: usize,
    /// Set while the built panels wait (ordered out) for their gating
    /// captures; see [`REVEAL_TIMEOUT`].
    reveal_since: Option<Instant>,
    /// Panels of a previous build, kept on screen until the new ones show.
    retired: Vec<Panel>,
    /// Torn down and off the screen, waiting to be freed.
    discarded: Vec<Panel>,
    tx: Sender<Landed>,
    rx: Receiver<Landed>,
    /// `batch`, shared with the capture workers so stale ones stop.
    current_batch: Arc<AtomicU64>,
    /// When the current batch was started, for the capture timing log.
    batch_started: Instant,
    /// The last image of everything captured ("window 42", "wallpaper 7",
    /// "menu bar 0,0"). A build shows these straight away, so the overview
    /// appears at once and the fresh captures fade in over them.
    cache: HashMap<String, CFRetained<CGImage>>,
    live: Live,
    /// Per panel: the windows whose cards are (nearly) on screen.
    visible: Vec<HashSet<i32>>,
    /// Upper bound on live streams, most important first (see [`Rank`]);
    /// each costs GPU time and memory while the overview is up. Config
    /// `overview.live_previews`.
    max_live: usize,
    /// Cards stream while on screen in the overview, or within this
    /// fraction of a display height of it; streams start and stop as cards
    /// scroll in and out. Config `overview.live_margin`.
    live_margin: f64,
    /// When to bring the live streams in line with what's visible, once a
    /// scroll has rested; see [`LIVE_SETTLE`].
    live_due: Option<Instant>,
    /// Whether `ScreenCaptureKit` streams have been started for the current
    /// build. Touch-down prebuilds stay at `progress == 0.0` so a cancelled
    /// touch or 4-finger Space swipe never flashes macOS's recording dot.
    live_synced: bool,
    /// Whether windows are captured at all (screen recording permission).
    /// Without it nothing is captured or streamed: cards are tiles with the
    /// app icon and title, the wallpaper comes from the desktop picture
    /// file, and the overview fades in instead of starting from the screen.
    previews: bool,
    /// App icons by process id, for the tiles.
    icons: HashMap<Pid, Option<CFRetained<CGImage>>>,
}

/// How long scrolling has to rest before the live streams follow it: while
/// it goes on, the running ones keep streaming instead of stopping and
/// starting as cards pass the edge.
const LIVE_SETTLE: Duration = Duration::from_millis(150);

/// Where the wallpaper and the menu bar go in one panel, filled in when
/// their captures land.
struct Screen {
    wallpaper: Vec<Retained<CALayer>>,
    /// Layers showing the wallpaper as heavily blurred as the backdrop.
    backdrop: Vec<Retained<CALayer>>,
    menubar: Vec<Retained<CALayer>>,
    /// Menu bar height, if the display has one.
    menubar_height: Option<f64>,
}

/// One display to show: its absolute CG bounds and the computed layout.
pub struct DisplayScene<'a, K> {
    pub display_id: u32,
    pub cg_bounds: Rect,
    pub layout: &'a DisplayLayout<K>,
}

/// What a card shows when windows can't be captured (no screen recording
/// permission): the window's title and its app's icon.
pub struct CardLabel {
    pub title: String,
    pub pid: Option<Pid>,
}

fn container_frames<K>(layout: &DisplayLayout<K>) -> (Rect, Rect) {
    let full = Rect::new(0.0, 0.0, layout.size.0, layout.size.1);
    (full, full.offset(0.0, -layout.scroll))
}

fn pan_frames(size: (f64, f64), pan: f64) -> (Rect, Rect) {
    let full = Rect::new(0.0, 0.0, size.0, size.1);
    (full, full.offset(pan, 0.0))
}

/// Picks at most `max` items from per-display lists so every display gets
/// its share: each display's best item first (by rank), then each one's
/// second best, and so on.
fn share<T>(per_display: impl Iterator<Item = Vec<(Rank, T)>>, max: usize) -> Vec<T> {
    let mut picked: Vec<(usize, Rank, usize, T)> = Vec::new();
    for (display, mut items) in per_display.enumerate() {
        items.sort_by_key(|(rank, _)| *rank);
        picked.extend(
            items
                .into_iter()
                .enumerate()
                .map(|(turn, (rank, item))| (turn, rank, display, item)),
        );
    }
    picked.sort_by_key(|(turn, rank, display, _)| (*turn, *rank, *display));
    picked
        .into_iter()
        .take(max)
        .map(|(.., item)| item)
        .collect()
}

fn visible_set<K>(layout: &DisplayLayout<K>, margin: f64) -> HashSet<i32> {
    super::visible_windows(layout, layout.size.1 * margin)
        .into_iter()
        .collect()
}

fn strip_cards<K>(
    strip: &super::StripPlacement<K>,
) -> impl Iterator<Item = (&super::WindowPlacement<K>, bool, Rank)> {
    let focus = strip
        .windows
        .iter()
        .filter(|w| !w.floating)
        .position(|w| w.focused);
    let mut column = 0;
    strip.windows.iter().map(move |window| {
        let distance = if window.floating {
            None
        } else {
            column += 1;
            focus.map(|f| f.abs_diff(column - 1))
        };
        let rank = Rank::of_card(strip.active, window.focused, window.floating, distance);
        (window, strip.active, rank)
    })
}

/// Cards in the order the panels hold them: every row's tiled windows,
/// then every floating window (drawn on top), paired with `(best, rank)`.
fn cards_of<K>(
    layout: &DisplayLayout<K>,
) -> impl Iterator<Item = (&super::WindowPlacement<K>, bool, Rank)> {
    let all = || layout.strips.iter().flat_map(strip_cards);
    all()
        .filter(|(w, ..)| !w.floating)
        .chain(all().filter(|(w, ..)| w.floating))
}

impl OverviewManager {
    pub fn new(mtm: MainThreadMarker, sender: EventSender) -> Self {
        let (tx, rx) = channel();
        let live_sender = sender.clone();
        Self {
            mtm,
            sender,
            panels: Vec::new(),
            generation: 0,
            batch: 0,
            targets: Vec::new(),
            outstanding: 0,
            gates: 0,
            reveal_since: None,
            retired: Vec::new(),
            discarded: Vec::new(),
            tx,
            rx,
            current_batch: Arc::new(AtomicU64::new(0)),
            batch_started: Instant::now(),
            cache: HashMap::new(),
            live: Live::new(mtm, live_sender),
            visible: Vec::new(),
            max_live: 4,
            live_margin: 0.0,
            live_due: None,
            live_synced: false,
            previews: true,
            icons: HashMap::new(),
        }
    }

    /// Turns window capture on or off (see [`Self::previews`]); applies from
    /// the next build.
    pub fn set_previews(&mut self, on: bool) {
        if self.previews != on {
            debug!(
                "overview: window previews {}",
                if on { "on" } else { "off" }
            );
            // Nothing captured under the other setting shows (a revoked
            // permission); the desktop picture stays cached while off.
            self.cache.clear();
        }
        self.previews = on;
    }

    pub fn previews(&self) -> bool {
        self.previews
    }

    /// Builds the layer trees at `progress` without blocking: every capture
    /// (windows, wallpaper, menu bar) runs on background threads. The panels
    /// are shown once the captures that make them match the real screen
    /// have landed (or after [`REVEAL_TIMEOUT`]); until then the real screen
    /// stays visible. Panels of a previous build stay up until then too, so
    /// a rebuild while open never flashes.
    #[allow(clippy::too_many_lines)]
    pub fn build<K>(
        &mut self,
        scenes: &[DisplayScene<'_, K>],
        info: &WindowServerInfo,
        labels: &HashMap<i32, CardLabel>,
        progress: f64,
    ) {
        let started = Instant::now();
        let old = std::mem::take(&mut self.panels);
        // A build superseding one still waiting: its panels never showed.
        if self.reveal_since.is_none() {
            self.retired.extend(old);
        } else {
            for panel in old {
                panel.window.orderOut(None);
            }
        }
        self.batch += 1;
        self.targets.clear();
        self.outstanding = 0;
        self.gates = 0;
        let primary_h = primary_screen_height(self.mtm);
        let mut jobs: Vec<(Rank, usize, Shot)> = Vec::new();
        self.visible.clear();
        if !self.previews {
            let live_pids: HashSet<Pid> = labels.values().filter_map(|label| label.pid).collect();
            self.icons.retain(|pid, _| live_pids.contains(pid));
            for pid in live_pids {
                self.icons.entry(pid).or_insert_with(|| app_icon(pid));
            }
        }

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for scene in scenes {
            let b = scene.cg_bounds;
            let at_corner = |f: &Rect| (f.x - b.x).abs() < 1.0 && (f.y - b.y).abs() < 1.0;
            let wallpaper = info.wallpapers.iter().find(|(_, f)| at_corner(f));
            let menubar = info.menubars.iter().find(|(_, f)| at_corner(f));
            if wallpaper.is_none() && self.previews {
                warn!(
                    "overview: no wallpaper found for display at ({}, {})",
                    b.x, b.y
                );
            }
            let mut screen = Screen {
                wallpaper: Vec::new(),
                backdrop: Vec::new(),
                menubar: Vec::new(),
                menubar_height: menubar.map(|(_, f)| f.h),
            };
            let Some(panel) = self.build_panel(scene, primary_h, &mut screen, labels) else {
                continue;
            };
            // Region captures are taken from below everything of ours on
            // this display: the previous build's panel if one is still up,
            // else this (still transparent) panel.
            let below = Some(
                self.retired
                    .get(self.panels.len())
                    .map_or(panel.window_number, |old| old.window_number),
            );
            panel.window.setAlphaValue(0.0);
            panel.window.orderFrontRegardless();
            if !self.previews {
                // Without the permission the wallpaper window can't be
                // captured (nor even found: window names are hidden), but
                // the desktop picture file can be read.
                if let Some(path) = desktop_picture(self.mtm, scene.display_id) {
                    let shot = Shot::File {
                        path: path.clone(),
                        blur: None,
                    };
                    let key = format!("desktop picture {path}");
                    let index = self.target(screen.wallpaper, false, false, false, &key);
                    jobs.push((Rank::Screen, index, shot));
                    let shot = Shot::File {
                        blur: Some((b.w, BACKDROP_BLUR)),
                        path,
                    };
                    let key = format!("{key} blurred");
                    let index = self.target(screen.backdrop, false, false, false, &key);
                    jobs.push((Rank::Backdrop, index, shot));
                }
            } else if let Some((id, _)) = wallpaper {
                let shot = Shot::Window {
                    id: *id,
                    best: true,
                };
                let key = format!("wallpaper {id}");
                let index = self.target(screen.wallpaper, false, true, false, &key);
                jobs.push((Rank::Screen, index, shot));
                let shot = Shot::Blurred {
                    id: *id,
                    width: b.w,
                    radius: BACKDROP_BLUR,
                };
                let key = format!("wallpaper {id} blurred");
                let index = self.target(screen.backdrop, false, true, false, &key);
                jobs.push((Rank::Backdrop, index, shot));
            }
            if let Some((_, rect)) = menubar.filter(|_| self.previews) {
                let shot = Shot::Region { rect: *rect, below };
                let key = format!("menu bar {},{}", rect.x, rect.y);
                let index = self.target(screen.menubar, false, true, false, &key);
                jobs.push((Rank::Row, index, shot));
            }
            let previews = self.previews;
            for card in panel.cards.iter().filter(|_| previews) {
                let shot = card.shot();
                let key = format!("window {}", card.window_id);
                let fresh = card.rank.wants_fresh();
                let index = self.target(vec![card.content.clone()], true, card.best, fresh, &key);
                if let Some(target) = self.targets.last_mut().and_then(Option::as_mut) {
                    target.window_id = Some(card.window_id);
                }
                jobs.push((card.rank, index, shot));
            }
            self.visible
                .push(visible_set(scene.layout, self.live_margin));
            if progress > 0.0 {
                for layer in panel.layers() {
                    layer.set(progress);
                }
            }
            self.panels.push(panel);
        }
        CATransaction::commit();

        // Forget windows that are gone.
        let live: std::collections::HashSet<_> = self
            .targets
            .iter()
            .flatten()
            .map(|target| target.key.clone())
            .collect();
        self.cache.retain(|key, _| live.contains(key));

        // Most important first: the focused window and its neighbours, then
        // the screen, the rest of the row, and the hidden rows last.
        self.current_batch.store(self.batch, Ordering::Relaxed);
        self.batch_started = Instant::now();
        jobs.sort_by_key(|(rank, ..)| *rank);
        let queue: Vec<_> = jobs
            .into_iter()
            .map(|(_, index, shot)| (index, shot))
            .collect();
        spawn_shots(
            &self.tx,
            &self.sender,
            &self.current_batch,
            self.batch,
            queue,
        );
        self.reveal_since = Some(Instant::now());
        self.live_due = None;
        self.live_synced = false;
        let built = started.elapsed();
        if progress > 0.0 {
            self.ensure_live_synced();
        }
        let synced = started.elapsed();
        self.poll_captures();
        debug!(
            "overview: build took {:?} (panels {built:?}, live {:?}, captures {:?})",
            started.elapsed(),
            synced.saturating_sub(built),
            started.elapsed().saturating_sub(synced),
        );
    }

    fn target(
        &mut self,
        layers: Vec<Retained<CALayer>>,
        clear: bool,
        gate: bool,
        fresh: bool,
        key: &str,
    ) -> usize {
        // A cached image goes in now, and then the capture no longer holds
        // the panels back, unless `fresh` asks for the real thing first.
        let cached = self.cache.get(key);
        if let Some(image) = cached {
            for layer in &layers {
                set_contents(layer, image);
                if clear {
                    layer.setBackgroundColor(None);
                }
            }
        }
        let gate = gate && (cached.is_none() || fresh);
        self.outstanding += 1;
        self.gates += usize::from(gate);
        self.targets.push(Some(Target {
            layers,
            clear_background: clear,
            gate,
            key: key.to_owned(),
            window_id: None,
        }));
        self.targets.len() - 1
    }

    /// Whether a sub-frame deadline (`REVEAL_TIMEOUT` or `LIVE_SETTLE`) needs
    /// `pump_events` to poll on a short timeout. Background captures, live
    /// content queries, and Core Animation springs all wake the event loop
    /// directly via `EventSender`, so they do not need 60 Hz polling.
    pub fn has_pending_deadline(&self) -> bool {
        self.reveal_since.is_some() || self.live_due.is_some()
    }

    /// Whether `overview_tick` still has periodic maintenance to run
    /// (deadlines, freeing torn-down panels, or dropping stopped `SCStream`s
    /// after [`STOP_GRACE`](super::live)).
    pub fn has_pending_tick(&self) -> bool {
        !self.discarded.is_empty() || self.has_pending_deadline() || self.live.has_stopping()
    }

    /// Applies the captures that landed, and shows the panels once they are
    /// ready. Never blocks.
    pub fn poll_captures(&mut self) {
        self.free_discarded();
        if let Some(due) = self.live_due
            && Instant::now() >= due
        {
            self.sync_live();
        }
        self.live.poll();
        let mut landed = false;
        // Once visible, fresh images cross-fade over the cached ones.
        let visible = self.reveal_since.is_none();
        CATransaction::begin();
        CATransaction::setDisableActions(!visible);
        CATransaction::setAnimationDuration(0.2);
        while let Ok((batch, index, image)) = self.rx.try_recv() {
            if batch != self.batch {
                continue;
            }
            let Some(target) = self.targets.get_mut(index).and_then(Option::take) else {
                continue;
            };
            landed = true;
            self.outstanding = self.outstanding.saturating_sub(1);
            if target.gate {
                self.gates = self.gates.saturating_sub(1);
            }
            debug!(
                "overview: {} landed after {:?}",
                target.key,
                self.batch_started.elapsed()
            );
            let Some(SendImage(image)) = image else {
                warn!("overview: no snapshot for {}", target.key);
                continue;
            };
            let live = target.window_id.is_some_and(|id| self.live.is_showing(id));
            if !live {
                for layer in &target.layers {
                    set_contents(layer, &image);
                    if target.clear_background {
                        layer.setBackgroundColor(None);
                    }
                }
            }
            self.cache.insert(target.key, image);
        }
        CATransaction::commit();
        if landed {
            CATransaction::flush();
        }
        if let Some(since) = self.reveal_since
            && (self.gates == 0 || since.elapsed() > REVEAL_TIMEOUT)
        {
            self.reveal_since = None;
            debug!(
                "overview: shown after {:?} ({} captures still pending)",
                since.elapsed(),
                self.gates
            );
            for panel in &self.panels {
                panel.window.setAlphaValue(1.0);
                panel.window.makeKeyWindow();
            }
            for panel in self.retired.drain(..) {
                panel.window.orderOut(None);
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_panel<K>(
        &self,
        scene: &DisplayScene<'_, K>,
        primary_h: f64,
        screen: &mut Screen,
        labels: &HashMap<i32, CardLabel>,
    ) -> Option<Panel> {
        let mtm = self.mtm;
        let b = scene.cg_bounds;
        let size = (b.w, b.h);
        let layout = scene.layout;
        let cocoa = NSRect::new(
            NSPoint::new(b.x, primary_h - b.y - b.h),
            NSSize::new(b.w, b.h),
        );
        let window: Retained<OverviewPanel> = unsafe {
            msg_send![
                OverviewPanel::alloc(mtm),
                initWithContentRect: cocoa,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        window.setLevel(NSScreenSaverWindowLevel);
        window.setHidesOnDeactivate(false);
        unsafe { window.setReleasedWhenClosed(false) };
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );

        let view = OverviewView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), cocoa.size),
            self.sender.clone(),
            (b.x, b.y),
        );
        view.setWantsLayer(true);
        window.setContentView(Some(&view));
        let Some(root) = view.layer() else {
            warn!("overview: content view has no backing layer");
            return None;
        };
        let scale = window.backingScaleFactor();
        #[allow(clippy::cast_possible_truncation)]
        let window_number = window.windowNumber() as i32;

        // Y-down so every rect maps 1:1 onto CG display-local coordinates.
        let flipped = CALayer::new();
        flipped.setGeometryFlipped(true);
        flipped.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(b.w, b.h)));
        root.addSublayer(&flipped);

        let full = Rect::new(0.0, 0.0, b.w, b.h);
        // niri's backdrop: the wallpaper, heavily blurred (pre-rendered, see
        // `blurred`). Opaque from the start: at progress 0 the active row's screen covers
        // it exactly, and as that shrinks the blur is what's revealed — the
        // real windows behind the panel never show through.
        // Without previews nothing matches the real screen at progress 0,
        // so the backdrop and every row fade in (and out) instead.
        let shown_at_rest = if self.previews { 1.0 } else { 0.0 };
        let backdrop = Animated::new(CALayer::new(), full, full, (shown_at_rest, 1.0));
        backdrop
            .layer
            .setBackgroundColor(Some(&color(0.08, 0.09, 0.11, 1.0)));
        backdrop
            .layer
            .setContentsGravity(unsafe { kCAGravityResizeAspectFill });
        screen.backdrop.push(backdrop.layer.clone());
        flipped.addSublayer(&backdrop.layer);
        backdrop.set(0.0);

        let tint = Animated::new(CALayer::new(), full, full, (0.0, 1.0));
        tint.layer
            .setBackgroundColor(Some(&color(0.0, 0.0, 0.0, 0.15)));
        flipped.addSublayer(&tint.layer);
        tint.set(0.0);

        let (container_rest, container_open) = container_frames(layout);
        let container = Animated::new(
            CALayer::new(),
            container_rest,
            container_open,
            (shown_at_rest, 1.0),
        );
        flipped.addSublayer(&container.layer);
        container.set(0.0);

        let mut rows = Vec::new();
        let mut tiled = Vec::new();
        let mut floating = Vec::new();
        for strip in &layout.strips {
            let mut viewport = Animated::new(
                CALayer::new(),
                strip.viewport_rest,
                strip.viewport_overview,
                (1.0, 1.0),
            );
            viewport
                .layer
                .setContentsGravity(unsafe { kCAGravityResizeAspectFill });
            viewport
                .layer
                .setBackgroundColor(Some(&color(0.2, 0.22, 0.26, 1.0)));
            screen.wallpaper.push(viewport.layer.clone());
            viewport.layer.setMasksToBounds(true);
            // The menu bar shrinks with the row's screen, fading out as it
            // goes. The viewport's bounds are the display's, so it sits at
            // its real place.
            let bar = screen.menubar_height.map(|height| {
                let rect = Rect::new(0.0, 0.0, viewport.base.0, height);
                let bar = Animated::new(CALayer::new(), rect, rect, (1.0, 0.0));
                screen.menubar.push(bar.layer.clone());
                bar.layer.setContentsGravity(unsafe { kCAGravityResize });
                viewport.layer.addSublayer(&bar.layer);
                bar.set(0.0);
                bar
            });
            viewport.corner = viewport.open_points(CARD_RADIUS);
            viewport
                .layer
                .setBorderColor(Some(&color(0.45, 0.65, 1.0, 0.9)));
            // The focused card carries the ring; outlining the row's "screen"
            // as well read as a second, misplaced focus border.
            viewport.set_ring(false);
            container.layer.addSublayer(&viewport.layer);
            viewport.set(0.0);

            let (pan_rest, pan_open) = pan_frames(size, strip.pan);
            let pan = Animated::new(CALayer::new(), pan_rest, pan_open, (1.0, 1.0));
            container.layer.addSublayer(&pan.layer);
            pan.set(0.0);

            for (window, best, rank) in strip_cards(strip) {
                let mut card =
                    Animated::new(CALayer::new(), window.rest, window.overview, (1.0, 1.0));
                card.layer.setMasksToBounds(true);
                card.corner = card.open_points(CARD_RADIUS);
                card.layer
                    .setBorderColor(Some(&color(0.45, 0.65, 1.0, 1.0)));
                card.set_ring(window.focused);
                let content = CALayer::new();
                content.setFrame(CGRect::new(
                    CGPoint::new(0.0, 0.0),
                    CGSize::new(card.base.0, card.base.1),
                ));
                content.setContentsGravity(unsafe { kCAGravityResize });
                content.setContentsScale(scale);
                // Tiled cards hold their slot with a placeholder until the
                // image lands. Floating cards sit on top of the rows, so one
                // whose capture never comes (or is blocked) would just cover
                // them: leave it clear instead.
                if !self.previews {
                    let label = labels.get(&window.window_id);
                    let icon = label
                        .and_then(|l| l.pid)
                        .and_then(|pid| self.icons.get(&pid))
                        .and_then(Option::as_deref);
                    add_tile(
                        &content,
                        &card,
                        label.map(|l| l.title.as_str()),
                        icon,
                        scale,
                    );
                } else if !window.floating {
                    content.setBackgroundColor(Some(&color(0.3, 0.32, 0.36, 1.0)));
                }
                card.layer.addSublayer(&content);
                let parent = if window.floating {
                    &container.layer
                } else {
                    &pan.layer
                };
                let entry = Card {
                    animated: card,
                    content,
                    window_id: window.window_id,
                    best,
                    rank,
                };
                if window.floating {
                    floating.push(entry);
                } else {
                    parent.addSublayer(&entry.animated.layer);
                    entry.animated.set(0.0);
                    tiled.push(entry);
                }
            }
            let label = row_label(strip.virtual_index, &strip.label, scale);
            container.layer.addSublayer(&label.layer);
            label.set(0.0);
            rows.push(RowLayers {
                viewport,
                bar,
                pan,
                label,
            });
        }
        // Floating windows go above every row.
        for card in &floating {
            container.layer.addSublayer(&card.animated.layer);
            card.animated.set(0.0);
        }
        tiled.extend(floating);

        Some(Panel {
            window,
            window_number,
            root: flipped,
            backdrop,
            tint,
            container,
            rows,
            cards: tiled,
            scale,
        })
    }

    fn layers(&self) -> impl Iterator<Item = &Animated> {
        self.panels.iter().flat_map(Panel::layers)
    }

    /// Jumps every layer to progress `p` (gesture scrubbing).
    pub fn set_progress(&mut self, p: f64) {
        if p > 0.0 {
            self.ensure_live_synced();
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for layer in self.layers() {
            layer.layer.removeAllAnimations();
            layer.set(p);
        }
        for panel in &self.panels {
            panel
                .root
                .removeAnimationForKey(&NSString::from_str(FADE_KEY));
        }
        CATransaction::commit();
        // Push the frame to the render server now rather than at the end of
        // the run-loop pass, so scrubbing doesn't trail the fingers.
        CATransaction::flush();
    }

    /// Springs every layer from `spring.from` to `spring.to` on the render
    /// server. When it settles, [`Event::OverviewTransitionDone`] arrives
    /// carrying the returned generation; an older generation means the
    /// transition was superseded and should be ignored.
    pub fn animate(&mut self, spring: &Spring) -> u64 {
        self.generation += 1;
        let generation = self.generation;
        let sender = self.sender.clone();
        let done = RcBlock::new(move || {
            _ = sender.send(Event::OverviewTransitionDone { generation });
        });
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        unsafe { CATransaction::setCompletionBlock(Some(&done)) };
        for layer in self.layers() {
            layer.animate(spring);
        }
        let closing = spring.to < 0.5;
        let shown = spring.visible_duration();
        for panel in &self.panels {
            panel
                .root
                .removeAnimationForKey(&NSString::from_str(FADE_KEY));
            if closing {
                fade_out_at_end(&panel.root, shown);
            }
        }
        CATransaction::commit();
        CATransaction::flush();
        if !closing {
            self.ensure_live_synced();
        }
        generation
    }

    /// Live preview limits, see the fields.
    pub fn set_live_limits(&mut self, max: usize, margin: f64) {
        self.max_live = max;
        self.live_margin = margin;
    }

    fn ensure_live_synced(&mut self) {
        if !self.live_synced {
            self.sync_live();
        }
    }

    /// Streams the windows whose cards are (nearly) on screen.
    fn sync_live(&mut self) {
        self.live_synced = true;
        self.live_due = None;
        if !self.previews || self.max_live == 0 {
            // Not even listing the windows: without permission that alone can
            // bring up the system prompt, and with `live_previews = 0` no
            // stream will be started anyway.
            self.live.set(Vec::new());
            return;
        }
        let per_display = self
            .panels
            .iter()
            .zip(&self.visible)
            .map(|(panel, visible)| {
                panel
                    .cards
                    .iter()
                    .filter(|card| visible.contains(&card.window_id))
                    .map(|card| {
                        // Sharp where the eye is, light elsewhere: cards away
                        // from the focus are small once open.
                        let scale = if card.rank.wants_fresh() {
                            panel.scale
                        } else {
                            1.0
                        };
                        let want = Wanted {
                            window_id: card.window_id,
                            layer: card.content.clone(),
                            scale,
                        };
                        (card.rank, want)
                    })
                    .collect()
            });
        self.live.set(share(per_display, self.max_live));
    }

    pub fn apply_offsets<K>(&mut self, index: usize, layout: &DisplayLayout<K>) {
        let visible = visible_set(layout, self.live_margin);
        if let Some(old) = self.visible.get_mut(index)
            && *old != visible
        {
            *old = visible;
            self.live_due = Some(Instant::now() + LIVE_SETTLE);
        }
        let Some(panel) = self.panels.get_mut(index) else {
            return;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        panel.container.overview = container_frames(layout).1;
        panel.container.layer.removeAllAnimations();
        panel.container.set(1.0);
        for (row, strip) in panel.rows.iter_mut().zip(&layout.strips) {
            row.pan.overview = pan_frames(layout.size, strip.pan).1;
            row.pan.layer.removeAllAnimations();
            row.pan.set(1.0);
        }
        CATransaction::commit();
        CATransaction::flush();
    }

    /// Springs an open overview to a new layout of the same windows (e.g.
    /// after a hotkey scrolled a strip or switched rows). Returns `false`
    /// when the set of rows or windows changed, so the caller rebuilds.
    pub fn relayout<K>(&mut self, layouts: &[&DisplayLayout<K>], spring: &Spring) -> bool {
        if self.panels.len() != layouts.len() {
            return false;
        }
        for (panel, layout) in self.panels.iter().zip(layouts) {
            let same_cards = panel
                .cards
                .iter()
                .map(|c| c.window_id)
                .eq(cards_of(layout).map(|(w, ..)| w.window_id));
            if panel.rows.len() != layout.strips.len() || !same_cards {
                return false;
            }
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (panel, layout) in self.panels.iter_mut().zip(layouts) {
            let (rest, open) = container_frames(layout);
            panel.container.morph(rest, open, spring);
            for (row, strip) in panel.rows.iter_mut().zip(&layout.strips) {
                row.viewport.set_ring(false);
                row.viewport
                    .morph(strip.viewport_rest, strip.viewport_overview, spring);
                let (rest, open) = pan_frames(layout.size, strip.pan);
                row.pan.morph(rest, open, spring);
                row.label.morph(strip.label, strip.label, spring);
            }
            for (card, (window, best, rank)) in panel.cards.iter_mut().zip(cards_of(layout)) {
                card.best = best;
                card.rank = rank;
                card.animated.set_ring(window.focused);
                card.animated.morph(window.rest, window.overview, spring);
            }
        }
        CATransaction::commit();
        // Hand the morph animations to the render server before `sync_live` or
        // the next focus keybind's synchronous AX calls run on the main thread.
        CATransaction::flush();
        self.visible = layouts
            .iter()
            .map(|layout| visible_set(layout, self.live_margin))
            .collect();
        self.sync_live();
        true
    }

    /// Re-targets the rest frames (e.g. after picking another row) without
    /// moving anything yet.
    pub fn retarget<K>(&mut self, layouts: &[&DisplayLayout<K>]) {
        for (panel, layout) in self.panels.iter_mut().zip(layouts) {
            for (row, strip) in panel.rows.iter_mut().zip(&layout.strips) {
                row.viewport.rest = strip.viewport_rest;
            }
            for (card, (window, ..)) in panel.cards.iter_mut().zip(cards_of(layout)) {
                card.animated.rest = window.rest;
            }
        }
    }

    pub fn teardown(&mut self) {
        let started = Instant::now();
        // Off the screen first; the layers and images they hold are freed on
        // the next pass (see `free_discarded`), not while the overlay is
        // still in front of the windows.
        for panel in self.panels.drain(..).chain(self.retired.drain(..)) {
            panel.window.orderOut(None);
            self.discarded.push(panel);
        }
        CATransaction::flush();
        let hidden = started.elapsed();
        self.live_due = None;
        self.live_synced = false;
        _ = self.sender.send(Event::OverviewCaptureLanded);
        self.live.stop_all();
        // Whatever is still in flight lands in an old batch and is dropped.
        self.batch += 1;
        self.current_batch.store(self.batch, Ordering::Relaxed);
        self.targets.clear();
        self.outstanding = 0;
        self.gates = 0;
        self.reveal_since = None;
        debug!(
            "overview: teardown took {:?} (hidden after {hidden:?})",
            started.elapsed()
        );
    }

    /// Frees the panels [`Self::teardown`] took off the screen.
    fn free_discarded(&mut self) {
        if self.discarded.is_empty() {
            return;
        }
        let started = Instant::now();
        self.discarded.clear();
        debug!("overview: freeing panels took {:?}", started.elapsed());
    }
}

/// Height of the display whose Cocoa frame sits at the origin; anchors the
/// CG ↔ Cocoa Y flip.
fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    let mut fallback = None;
    for screen in &screens {
        let frame = screen.frame();
        fallback.get_or_insert(frame.size.height);
        if frame.origin.x == 0.0 && frame.origin.y == 0.0 {
            return frame.size.height;
        }
    }
    fallback.unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_previews_are_shared_between_displays() {
        let laptop = vec![
            (Rank::Row, "laptop row"),
            (Rank::Focused, "laptop focused"),
            (Rank::Neighbour, "laptop neighbour"),
        ];
        let monitor = vec![(Rank::Hidden, "monitor hidden"), (Rank::Row, "monitor row")];
        let picked = share([laptop, monitor].into_iter(), 4);
        assert_eq!(
            picked,
            [
                "laptop focused",
                "monitor row",
                "laptop neighbour",
                "monitor hidden"
            ]
        );
        assert_eq!(
            share(std::iter::once(vec![(Rank::Row, 1)]), 0),
            Vec::<i32>::new()
        );
    }

    #[test]
    fn focused_window_and_neighbours_are_captured_first() {
        assert_eq!(Rank::of_card(true, true, false, Some(0)), Rank::Focused);
        assert_eq!(Rank::of_card(true, false, false, Some(1)), Rank::Neighbour);
        assert_eq!(Rank::of_card(true, false, false, Some(2)), Rank::Row);
        assert_eq!(Rank::of_card(true, false, false, None), Rank::Row);
        // A focused floating window has no column but still goes first.
        assert_eq!(Rank::of_card(true, true, true, None), Rank::Focused);
        // Other floating windows beat tiled cards further from the focus.
        assert_eq!(Rank::of_card(true, false, true, None), Rank::Floating);
        assert!(Rank::Floating < Rank::Row && Rank::Neighbour < Rank::Floating);
        assert!(!Rank::Floating.wants_fresh());
        assert_eq!(Rank::of_card(false, true, false, Some(0)), Rank::Hidden);
        assert_eq!(Rank::of_card(false, false, true, None), Rank::Hidden);
        let mut ranks = [
            Rank::Hidden,
            Rank::Row,
            Rank::Screen,
            Rank::Neighbour,
            Rank::Focused,
        ];
        ranks.sort();
        assert_eq!(ranks.first(), Some(&Rank::Focused));
        assert!(Rank::Focused.wants_fresh() && Rank::Neighbour.wants_fresh());
        assert!(!Rank::Row.wants_fresh());
    }

    #[test]
    fn active_row_cards_always_capture_their_own_window_not_a_screen_region() {
        use crate::overview::{Metrics, StripInput, WindowInput, layout_display};

        // Reproduces the 3-window strip (`[Gmail, Meet, Ghostty]` on 3840x2160)
        // where `Meet` sits inside the display bounds while neighbours sit off
        // left/right or slide underneath: every card must capture its own
        // `window_id` (`Shot::Window`), never a screen region (`Shot::Region`),
        // and `cards_of` recomputes ranks when focus moves right.
        let make_layout = |focused_idx: usize| {
            let strips = vec![StripInput {
                virtual_index: 0,
                active: true,
                windows: vec![
                    WindowInput {
                        key: 1,
                        window_id: 101,
                        layout_frame: Rect::new(-1572.0, 0.0, 2328.0, 2160.0),
                        real_frame: Some(Rect::new(-1572.0, 0.0, 2328.0, 2160.0)),
                        floating: false,
                        focused: focused_idx == 0,
                    },
                    WindowInput {
                        key: 2,
                        window_id: 102,
                        layout_frame: Rect::new(756.0, 0.0, 2328.0, 2160.0),
                        real_frame: Some(Rect::new(350.0, 0.0, 2328.0, 2160.0)),
                        floating: false,
                        focused: focused_idx == 1,
                    },
                    WindowInput {
                        key: 3,
                        window_id: 103,
                        layout_frame: Rect::new(3084.0, 0.0, 2328.0, 2160.0),
                        real_frame: Some(Rect::new(3835.0, 0.0, 2328.0, 2160.0)),
                        floating: false,
                        focused: focused_idx == 2,
                    },
                ],
            }];
            layout_display((3840.0, 2160.0), &strips, 1.0, &Metrics::default())
        };

        let layout = make_layout(0);
        let initial_ranks: Vec<_> = cards_of(&layout)
            .map(|(window, best, rank)| {
                let card = Card {
                    animated: Animated::new(
                        CALayer::new(),
                        window.rest,
                        window.overview,
                        (1.0, 1.0),
                    ),
                    content: CALayer::new(),
                    window_id: window.window_id,
                    best,
                    rank,
                };
                assert_eq!(
                    card.shot(),
                    Shot::Window {
                        id: window.window_id,
                        best: true,
                    },
                    "window {} must capture by window_id, not display region",
                    window.window_id,
                );
                (window.window_id, best, rank)
            })
            .collect();
        assert_eq!(
            initial_ranks,
            [
                (101, true, Rank::Focused),
                (102, true, Rank::Neighbour),
                (103, true, Rank::Row),
            ]
        );

        // Moving focus all the way to the rightmost window updates `cards_of`
        // so `relayout` promotes the rightmost card to `Rank::Focused`.
        let moved = make_layout(2);
        let updated_ranks: Vec<_> = cards_of(&moved)
            .map(|(w, best, rank)| (w.window_id, best, rank))
            .collect();
        assert_eq!(
            updated_ranks,
            [
                (101, true, Rank::Row),
                (102, true, Rank::Neighbour),
                (103, true, Rank::Focused),
            ]
        );
    }
}
