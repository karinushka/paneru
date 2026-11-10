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
//! Window contents are static snapshots from `CGWindowListCreateImage`.
//! Live previews would swap in a `ScreenCaptureKit` stream per card without
//! changing the layer structure.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSPanel, NSScreen, NSScreenSaverWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGColor, CGImage, CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CALayer, CAMediaTiming as _, CASpringAnimation, CATransaction, kCAGravityResize,
    kCAGravityResizeAspectFill,
};
use tracing::{debug, warn};

use super::{DisplayLayout, Rect, Spring};
use crate::events::{Event, EventSender};

/// Blur of the wallpaper backdrop, in points.
const BACKDROP_BLUR: f64 = 40.0;
/// Corner radius and focus ring width of a card when fully open, in points.
const CARD_RADIUS: f64 = 10.0;
const RING_WIDTH: f64 = 2.5;

// ── WindowServer queries ────────────────────────────────────────────────

unsafe extern "C" {
    // Deprecated (and hidden from the SDK headers) since macOS 14 in favour of
    // ScreenCaptureKit, but still exported and working. Spike-only shortcut.
    fn CGWindowListCreateImage(
        screen_bounds: CGRect,
        list_option: u32,
        window_id: u32,
        image_option: u32,
    ) -> *mut CGImage;
}

const LIST_ON_SCREEN_ONLY: u32 = 1 << 0;
const LIST_ON_SCREEN_BELOW_WINDOW: u32 = 1 << 2;
const LIST_INCLUDING_WINDOW: u32 = 1 << 3;
const IMAGE_BOUNDS_IGNORE_FRAMING: u32 = 1 << 0;
const IMAGE_BEST_RESOLUTION: u32 = 1 << 3;
const IMAGE_NOMINAL_RESOLUTION: u32 = 1 << 4;

fn snapshot(window_id: i32, best: bool) -> Option<CFRetained<CGImage>> {
    let null = CGRect::new(
        CGPoint::new(f64::INFINITY, f64::INFINITY),
        CGSize::new(0.0, 0.0),
    );
    let resolution = if best {
        IMAGE_BEST_RESOLUTION
    } else {
        IMAGE_NOMINAL_RESOLUTION
    };
    let id = u32::try_from(window_id).ok()?;
    let raw = unsafe {
        CGWindowListCreateImage(
            null,
            LIST_INCLUDING_WINDOW,
            id,
            IMAGE_BOUNDS_IGNORE_FRAMING | resolution,
        )
    };
    std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

// ── Background captures ─────────────────────────────────────────────────

/// One capture to run off the main thread.
#[derive(Clone, Copy)]
enum Shot {
    Window {
        id: i32,
        best: bool,
    },
    /// A composited screen region, optionally from below one of our panels.
    Region {
        rect: Rect,
        below: Option<i32>,
    },
}

impl Shot {
    fn take(self) -> Option<CFRetained<CGImage>> {
        match self {
            Self::Window { id, best } => snapshot(id, best),
            Self::Region { rect, below } => snapshot_region(&rect, below),
        }
    }
}

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
}

/// How long the panels wait for gating captures before showing anyway.
const REVEAL_TIMEOUT: Duration = Duration::from_millis(250);

/// A capture result: (batch, target index, retained `CGImage` address or 0).
/// The image travels as an address: a `CGImage` is immutable and safe to
/// share between threads, the wrapper type just doesn't say so.
type Landed = (u64, usize, usize);

fn spawn_shots(tx: &Sender<Landed>, batch: u64, shots: Vec<(usize, Shot)>) {
    let tx = tx.clone();
    std::thread::spawn(move || {
        for (index, shot) in shots {
            let addr = shot
                .take()
                .map_or(0, |image| CFRetained::into_raw(image).as_ptr() as usize);
            if let Err(err) = tx.send((batch, index, addr)) {
                release_landed(err.0.2);
                return;
            }
        }
    });
}

fn release_landed(addr: usize) -> Option<CFRetained<CGImage>> {
    std::ptr::NonNull::new(addr as *mut CGImage).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// Everything on screen inside `rect` (absolute CG), composited — e.g. the
/// menu bar together with every status item, which are separate windows.
/// With `below`, only what sits under that window (our own panel).
fn snapshot_region(rect: &Rect, below: Option<i32>) -> Option<CFRetained<CGImage>> {
    let bounds = CGRect::new(CGPoint::new(rect.x, rect.y), CGSize::new(rect.w, rect.h));
    let (option, window) = match below.and_then(|id| u32::try_from(id).ok()) {
        Some(id) => (LIST_ON_SCREEN_BELOW_WINDOW, id),
        None => (LIST_ON_SCREEN_ONLY, 0),
    };
    let raw = unsafe { CGWindowListCreateImage(bounds, option, window, IMAGE_BEST_RESOLUTION) };
    std::ptr::NonNull::new(raw).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// What the `WindowServer` says is on screen right now, in absolute CG
/// coordinates. Its frames are what the snapshots are cut to, so they are
/// the exact place a card has to start from to cover its window.
#[derive(Default)]
pub struct WindowServerInfo {
    pub frames: HashMap<i32, Rect>,
    /// Dock's per-display wallpaper windows.
    wallpapers: Vec<(i32, Rect)>,
    /// The `WindowServer`'s per-display menu bars.
    menubars: Vec<(i32, Rect)>,
}

fn get(dict: &AnyObject, key: &str) -> Option<Retained<AnyObject>> {
    let key = NSString::from_str(key);
    unsafe { msg_send![dict, objectForKey: &*key] }
}

fn number(object: &AnyObject) -> f64 {
    unsafe { msg_send![object, doubleValue] }
}

pub fn window_server_info() -> WindowServerInfo {
    let mut info = WindowServerInfo::default();
    let Some(list) =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)
    else {
        return info;
    };
    // SAFETY: a CFArray of CFDictionary is toll-free bridged to NSArray.
    let array = unsafe { &*(&raw const *list).cast::<NSArray<AnyObject>>() };
    for entry in array {
        let Some(id) = get(&entry, "kCGWindowNumber").map(|n| number(&n)) else {
            continue;
        };
        let Some(bounds) = get(&entry, "kCGWindowBounds") else {
            continue;
        };
        let field = |key| get(&bounds, key).map_or(0.0, |n| number(&n));
        let frame = Rect::new(field("X"), field("Y"), field("Width"), field("Height"));
        #[allow(clippy::cast_possible_truncation)]
        let id = id as i32;
        let text = |key| {
            get(&entry, key)
                .and_then(|s| s.downcast::<NSString>().ok())
                .map(|s| s.to_string())
                .unwrap_or_default()
        };
        let (owner, name) = (text("kCGWindowOwnerName"), text("kCGWindowName"));
        if owner == "Dock" && name.starts_with("Wallpaper-") {
            info.wallpapers.push((id, frame));
        } else if owner == "Window Server" && name == "Menubar" {
            info.menubars.push((id, frame));
        } else {
            info.frames.insert(id, frame);
        }
    }
    info
}

// ── Small CA helpers ────────────────────────────────────────────────────

/// A `CIGaussianBlur` filter for `CALayer.filters`, if Core Image is there.
fn gaussian_blur(radius: f64) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get(c"CIFilter")?;
    let name = NSString::from_str("CIGaussianBlur");
    let filter: Option<Retained<AnyObject>> = unsafe { msg_send![class, filterWithName: &*name] };
    let filter = filter?;
    let key = NSString::from_str("inputRadius");
    let value = ns_number(radius);
    let _: () = unsafe { msg_send![&*filter, setValue: &*value, forKey: &*key] };
    Some(filter)
}

fn color(r: f64, g: f64, b: f64, a: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(r, g, b, a)
}

fn centre(r: &Rect) -> CGPoint {
    CGPoint::new(r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn ns_value_point(p: CGPoint) -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSValue), valueWithPoint: p] }
}

fn ns_number(v: f64) -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSNumber), numberWithDouble: v] }
}

fn set_key_path(layer: &CALayer, key: &str, value: f64) {
    let key = NSString::from_str(key);
    let value = ns_number(value);
    unsafe {
        let _: () = msg_send![layer, setValue: &*value, forKeyPath: &*key];
    }
}

fn spring_anim(
    key: &str,
    from: &AnyObject,
    to: &AnyObject,
    spring: &Spring,
) -> Retained<CASpringAnimation> {
    let anim = CASpringAnimation::animationWithKeyPath(Some(&NSString::from_str(key)));
    anim.setMass(1.0);
    anim.setStiffness(spring.stiffness());
    anim.setDamping(spring.damping());
    anim.setInitialVelocity(spring.normalized_velocity());
    unsafe {
        anim.setFromValue(Some(from));
        anim.setToValue(Some(to));
    }
    anim.setDuration(anim.settlingDuration());
    anim
}

fn set_contents(layer: &CALayer, image: &CGImage) {
    // SAFETY: a CGImage is a CF object, usable as `id` layer contents.
    let object = unsafe { &*(&raw const *image).cast::<AnyObject>() };
    unsafe { layer.setContents(Some(object)) };
}

/// A layer whose frame interpolates between `rest` (progress 0) and
/// `overview` (progress 1). Bounds stay at `base`; the frame is produced
/// with `position` and `transform.scale`.
struct Animated {
    layer: Retained<CALayer>,
    rest: Rect,
    overview: Rect,
    /// Opacity at progress 0 and 1.
    opacity: (f64, f64),
    base: (f64, f64),
    /// Highlight ring width in bounds space when fully open; it grows in with
    /// progress so nothing outlines the real screen at the start.
    ring: f64,
    /// Corner radius in bounds space when fully open; grows in like `ring`
    /// so the full-size frame at progress 0 has square corners.
    corner: f64,
}

impl Animated {
    fn new(layer: Retained<CALayer>, rest: Rect, overview: Rect, opacity: (f64, f64)) -> Self {
        let base = if rest.w >= 1.0 && rest.h >= 1.0 {
            (rest.w, rest.h)
        } else {
            (overview.w.max(1.0), overview.h.max(1.0))
        };
        layer.setBounds(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(base.0, base.1),
        ));
        Self {
            layer,
            rest,
            overview,
            opacity,
            base,
            ring: 0.0,
            corner: 0.0,
        }
    }

    fn set_ring(&mut self, on: bool) {
        self.ring = if on {
            self.open_points(RING_WIDTH)
        } else {
            0.0
        };
    }

    /// Bounds-space length that renders as `points` when fully open.
    fn open_points(&self, points: f64) -> f64 {
        points * self.base.0 / self.overview.w.max(1.0)
    }

    fn frame(&self, p: f64) -> Rect {
        self.rest.lerp(&self.overview, p)
    }

    fn opacity_at(&self, p: f64) -> f64 {
        (self.opacity.0 + (self.opacity.1 - self.opacity.0) * p).clamp(0.0, 1.0)
    }

    fn apply(&self, f: &Rect, opacity: f64) {
        self.layer.setPosition(centre(f));
        set_key_path(&self.layer, "transform.scale.x", f.w / self.base.0);
        set_key_path(&self.layer, "transform.scale.y", f.h / self.base.1);
        #[allow(clippy::cast_possible_truncation)]
        self.layer.setOpacity(opacity as f32);
    }

    fn set(&self, p: f64) {
        self.apply(&self.frame(p), self.opacity_at(p));
        let p = p.clamp(0.0, 1.0);
        self.layer.setBorderWidth(self.ring * p);
        self.layer.setCornerRadius(self.corner * p);
    }

    fn spring_between(&self, a: &Rect, b: &Rect, opacity: (f64, f64), spring: &Spring) {
        self.layer.removeAllAnimations();
        self.apply(b, opacity.1);
        let anims = [
            spring_anim(
                "position",
                &ns_value_point(centre(a)),
                &ns_value_point(centre(b)),
                spring,
            ),
            spring_anim(
                "transform.scale.x",
                &ns_number(a.w / self.base.0),
                &ns_number(b.w / self.base.0),
                spring,
            ),
            spring_anim(
                "transform.scale.y",
                &ns_number(a.h / self.base.1),
                &ns_number(b.h / self.base.1),
                spring,
            ),
        ];
        for anim in &anims {
            self.layer.addAnimation_forKey(anim, None);
        }
        if (opacity.0 - opacity.1).abs() > f64::EPSILON {
            let fade = spring_anim(
                "opacity",
                &ns_number(opacity.0),
                &ns_number(opacity.1),
                spring,
            );
            self.layer.addAnimation_forKey(&fade, None);
        }
    }

    /// Springs from progress `spring.from` to `spring.to`.
    fn animate(&self, spring: &Spring) {
        let (a, b) = (self.frame(spring.from), self.frame(spring.to));
        let opacity = (self.opacity_at(spring.from), self.opacity_at(spring.to));
        self.spring_between(&a, &b, opacity, spring);
        for (key, full) in [("borderWidth", self.ring), ("cornerRadius", self.corner)] {
            if full <= 0.0 {
                continue;
            }
            let at = |p: f64| full * p.clamp(0.0, 1.0);
            set_key_path(&self.layer, key, at(spring.to));
            let grow = spring_anim(
                key,
                &ns_number(at(spring.from)),
                &ns_number(at(spring.to)),
                spring,
            );
            self.layer.addAnimation_forKey(&grow, None);
        }
    }

    /// Moves the open (and rest) frame to new targets, springing from where
    /// it is now. Assumes the overview is fully open.
    fn morph(&mut self, rest: Rect, overview: Rect, spring: &Spring) {
        let from = self.overview;
        self.rest = rest;
        self.overview = overview;
        if from != overview {
            let o = self.opacity_at(1.0);
            self.spring_between(&from, &overview, (o, o), spring);
        }
        self.layer.setBorderWidth(self.ring);
        self.layer.setCornerRadius(self.corner);
    }
}

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

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let point = self.cg_point(event);
            // Trackpads report points; wheels report lines.
            let unit = if event.hasPreciseScrollingDeltas() { 1.0 } else { 12.0 };
            _ = self.ivars().sender.send(Event::OverviewScroll {
                point,
                dx: event.scrollingDeltaX() * unit,
                dy: event.scrollingDeltaY() * unit,
            });
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Escape or Return.
            if matches!(event.keyCode(), 53 | 36) {
                _ = self.ivars().sender.send(Event::OverviewDismiss);
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
    window_id: i32,
    /// On the active row: captured at full resolution, before showing.
    best: bool,
}

struct RowLayers {
    viewport: Animated,
    /// The menu bar inside the row's screen: there at the start so the
    /// overlay matches the real screen, gone once open.
    bar: Option<Animated>,
    pan: Animated,
}

struct Panel {
    window: Retained<OverviewPanel>,
    window_number: i32,
    backdrop: Animated,
    tint: Animated,
    container: Animated,
    rows: Vec<RowLayers>,
    cards: Vec<Card>,
}

impl Panel {
    fn layers(&self) -> impl Iterator<Item = &Animated> {
        [&self.backdrop, &self.tint, &self.container]
            .into_iter()
            .chain(self.rows.iter().flat_map(|row| {
                [Some(&row.viewport), row.bar.as_ref(), Some(&row.pan)]
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
    tx: Sender<Landed>,
    rx: Receiver<Landed>,
    /// The last image of everything captured ("window 42", "wallpaper 7",
    /// "menu bar 0,0"). A build shows these straight away, so the overview
    /// appears at once and the fresh captures fade in over them.
    cache: HashMap<String, CFRetained<CGImage>>,
}

/// Where the wallpaper and the menu bar go in one panel, filled in when
/// their captures land.
struct Screen {
    wallpaper: Vec<Retained<CALayer>>,
    menubar: Vec<Retained<CALayer>>,
    /// Menu bar height, if the display has one.
    menubar_height: Option<f64>,
}

/// One display to show: its absolute CG bounds and the computed layout.
pub struct DisplayScene<'a, K> {
    pub cg_bounds: Rect,
    pub layout: &'a DisplayLayout<K>,
}

fn container_frames<K>(layout: &DisplayLayout<K>) -> (Rect, Rect) {
    let full = Rect::new(0.0, 0.0, layout.size.0, layout.size.1);
    (full, full.offset(0.0, -layout.scroll))
}

fn pan_frames(size: (f64, f64), pan: f64) -> (Rect, Rect) {
    let full = Rect::new(0.0, 0.0, size.0, size.1);
    (full, full.offset(pan, 0.0))
}

/// Cards in the order the panels hold them: every row's tiled windows,
/// then every floating window (drawn on top).
fn card_order<K>(layout: &DisplayLayout<K>) -> impl Iterator<Item = &super::WindowPlacement<K>> {
    let windows = || layout.strips.iter().flat_map(|s| s.windows.iter());
    windows()
        .filter(|w| !w.floating)
        .chain(windows().filter(|w| w.floating))
}

impl OverviewManager {
    pub fn new(mtm: MainThreadMarker, sender: EventSender) -> Self {
        let (tx, rx) = channel();
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
            tx,
            rx,
            cache: HashMap::new(),
        }
    }

    /// Builds the layer trees at `progress` without blocking: every capture
    /// (windows, wallpaper, menu bar) runs on background threads. The panels
    /// are shown once the captures that make them match the real screen
    /// have landed (or after [`REVEAL_TIMEOUT`]); until then the real screen
    /// stays visible. Panels of a previous build stay up until then too, so
    /// a rebuild while open never flashes.
    pub fn build<K>(
        &mut self,
        scenes: &[DisplayScene<'_, K>],
        info: &WindowServerInfo,
        progress: f64,
    ) {
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
        let below = self.retired.first().map(|panel| panel.window_number);
        let primary_h = primary_screen_height(self.mtm);
        let mut gating = Vec::new();
        let mut later = Vec::new();

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for scene in scenes {
            let b = scene.cg_bounds;
            let at_corner = |f: &Rect| (f.x - b.x).abs() < 1.0 && (f.y - b.y).abs() < 1.0;
            let wallpaper = info.wallpapers.iter().find(|(_, f)| at_corner(f));
            let menubar = info.menubars.iter().find(|(_, f)| at_corner(f));
            if wallpaper.is_none() {
                warn!(
                    "overview: no wallpaper found for display at ({}, {})",
                    b.x, b.y
                );
            }
            let mut screen = Screen {
                wallpaper: Vec::new(),
                menubar: Vec::new(),
                menubar_height: menubar.map(|(_, f)| f.h),
            };
            let Some(panel) = self.build_panel(scene, primary_h, &mut screen) else {
                continue;
            };
            if let Some((id, _)) = wallpaper {
                let shot = Shot::Window {
                    id: *id,
                    best: true,
                };
                gating.push((
                    self.target(screen.wallpaper, false, true, &format!("wallpaper {id}")),
                    shot,
                ));
            }
            if let Some((_, rect)) = menubar {
                let shot = Shot::Region { rect: *rect, below };
                let key = format!("menu bar {},{}", rect.x, rect.y);
                gating.push((self.target(screen.menubar, false, true, &key), shot));
            }
            for card in &panel.cards {
                let shot = Shot::Window {
                    id: card.window_id,
                    best: card.best,
                };
                let key = format!("window {}", card.window_id);
                let index = self.target(vec![card.animated.layer.clone()], true, card.best, &key);
                if card.best {
                    gating.push((index, shot));
                } else {
                    later.push((index, shot));
                }
            }
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

        // Each gating capture is a round trip to the WindowServer of tens of
        // milliseconds; they overlap well, so each gets its own thread. The
        // rest share one, behind them.
        for shot in gating {
            spawn_shots(&self.tx, self.batch, vec![shot]);
        }
        spawn_shots(&self.tx, self.batch, later);
        self.reveal_since = Some(Instant::now());
        self.poll_captures();
    }

    fn target(
        &mut self,
        layers: Vec<Retained<CALayer>>,
        clear: bool,
        gate: bool,
        key: &str,
    ) -> usize {
        // A cached image goes in now, and then the capture no longer holds
        // the panels back.
        let cached = self.cache.get(key);
        if let Some(image) = cached {
            for layer in &layers {
                set_contents(layer, image);
                if clear {
                    layer.setBackgroundColor(None);
                }
            }
        }
        let gate = gate && cached.is_none();
        self.outstanding += 1;
        self.gates += usize::from(gate);
        self.targets.push(Some(Target {
            layers,
            clear_background: clear,
            gate,
            key: key.to_owned(),
        }));
        self.targets.len() - 1
    }

    /// Whether captures are still in flight or the panels wait to show, so
    /// the main loop has to keep ticking.
    pub fn has_pending_captures(&self) -> bool {
        self.outstanding > 0 || self.reveal_since.is_some()
    }

    /// Applies the captures that landed, and shows the panels once they are
    /// ready. Never blocks.
    pub fn poll_captures(&mut self) {
        let mut landed = false;
        // Once visible, fresh images cross-fade over the cached ones.
        let visible = self.reveal_since.is_none();
        CATransaction::begin();
        CATransaction::setDisableActions(!visible);
        CATransaction::setAnimationDuration(0.2);
        while let Ok((batch, index, addr)) = self.rx.try_recv() {
            let image = release_landed(addr);
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
            let Some(image) = image else {
                warn!("overview: no snapshot for {}", target.key);
                continue;
            };
            for layer in &target.layers {
                set_contents(layer, &image);
                if target.clear_background {
                    layer.setBackgroundColor(None);
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
                panel.window.orderFrontRegardless();
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
        // niri's backdrop: the wallpaper, heavily blurred. Oversized so the blur's soft edges stay off-screen.
        let bleed = BACKDROP_BLUR * 3.0;
        let backdrop_rect = Rect::new(-bleed, -bleed, b.w + 2.0 * bleed, b.h + 2.0 * bleed);
        // Opaque from the start: at progress 0 the active row's screen covers
        // it exactly, and as that shrinks the blur is what's revealed — the
        // real windows behind the panel never show through.
        let backdrop = Animated::new(CALayer::new(), backdrop_rect, backdrop_rect, (1.0, 1.0));
        backdrop
            .layer
            .setBackgroundColor(Some(&color(0.08, 0.09, 0.11, 1.0)));
        backdrop
            .layer
            .setContentsGravity(unsafe { kCAGravityResizeAspectFill });
        if let Some(blur) = gaussian_blur(BACKDROP_BLUR) {
            view.setLayerUsesCoreImageFilters(true);
            unsafe {
                backdrop
                    .layer
                    .setFilters(Some(&NSArray::from_retained_slice(&[blur])));
            }
            // Blur once, then reuse the bitmap.
            backdrop.layer.setShouldRasterize(true);
            backdrop.layer.setRasterizationScale(scale);
        }
        screen.wallpaper.push(backdrop.layer.clone());
        flipped.addSublayer(&backdrop.layer);
        backdrop.set(0.0);

        let tint = Animated::new(CALayer::new(), full, full, (0.0, 1.0));
        tint.layer
            .setBackgroundColor(Some(&color(0.0, 0.0, 0.0, 0.15)));
        flipped.addSublayer(&tint.layer);
        tint.set(0.0);

        let (container_rest, container_open) = container_frames(layout);
        let container = Animated::new(CALayer::new(), container_rest, container_open, (1.0, 1.0));
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

            for window in &strip.windows {
                let mut card =
                    Animated::new(CALayer::new(), window.rest, window.overview, (1.0, 1.0));
                card.layer.setMasksToBounds(true);
                card.corner = card.open_points(CARD_RADIUS);
                card.layer
                    .setBorderColor(Some(&color(0.45, 0.65, 1.0, 1.0)));
                card.set_ring(window.focused);
                card.layer.setContentsGravity(unsafe { kCAGravityResize });
                card.layer.setContentsScale(scale);
                card.layer
                    .setBackgroundColor(Some(&color(0.3, 0.32, 0.36, 1.0)));
                let parent = if window.floating {
                    &container.layer
                } else {
                    &pan.layer
                };
                let entry = Card {
                    animated: card,
                    window_id: window.window_id,
                    best: strip.active,
                };
                if window.floating {
                    floating.push(entry);
                } else {
                    parent.addSublayer(&entry.animated.layer);
                    entry.animated.set(0.0);
                    tiled.push(entry);
                }
            }
            rows.push(RowLayers { viewport, bar, pan });
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
            backdrop,
            tint,
            container,
            rows,
            cards: tiled,
        })
    }

    fn layers(&self) -> impl Iterator<Item = &Animated> {
        self.panels.iter().flat_map(Panel::layers)
    }

    /// Jumps every layer to progress `p` (gesture scrubbing).
    pub fn set_progress(&mut self, p: f64) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for layer in self.layers() {
            layer.layer.removeAllAnimations();
            layer.set(p);
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
        CATransaction::commit();
        generation
    }

    /// Applies a scroll or pan of display `index` immediately (the overview
    /// is open; trackpad scroll events arrive at display rate).
    pub fn apply_offsets<K>(&mut self, index: usize, layout: &DisplayLayout<K>) {
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
                .eq(card_order(layout).map(|w| w.window_id));
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
            }
            for (card, window) in panel.cards.iter_mut().zip(card_order(layout)) {
                card.animated.set_ring(window.focused);
                card.animated.morph(window.rest, window.overview, spring);
            }
        }
        CATransaction::commit();
        true
    }

    /// Re-targets the rest frames (e.g. after picking another row) without
    /// moving anything yet.
    pub fn retarget<K>(&mut self, layouts: &[&DisplayLayout<K>]) {
        for (panel, layout) in self.panels.iter_mut().zip(layouts) {
            for (row, strip) in panel.rows.iter_mut().zip(&layout.strips) {
                row.viewport.rest = strip.viewport_rest;
            }
            for (card, window) in panel.cards.iter_mut().zip(card_order(layout)) {
                card.animated.rest = window.rest;
            }
        }
    }

    pub fn teardown(&mut self) {
        for panel in self.panels.drain(..).chain(self.retired.drain(..)) {
            panel.window.orderOut(None);
        }
        // Whatever is still in flight lands in an old batch and is dropped.
        self.batch += 1;
        self.targets.clear();
        self.outstanding = 0;
        self.gates = 0;
        self.reveal_since = None;
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
