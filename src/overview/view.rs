//! Native projection of the overview: one borderless, non-activating panel
//! per display above everything, holding a Core Animation layer tree.
//!
//! ```text
//! panel (WindowServer background blur, ramped with progress)
//! └─ root
//!    ├─ tint                       opacity 0 → 1
//!    └─ container                  vertical scroll
//!       ├─ per row: viewport       the row's "screen": wallpaper, shrinks
//!       │           label          fades in
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
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
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
    CALayer, CAMediaTiming as _, CASpringAnimation, CATextLayer, CATransaction, kCAAlignmentLeft,
    kCAGravityResize, kCAGravityResizeAspectFill, kCATruncationEnd,
};
use tracing::{debug, warn};

use super::{DisplayLayout, Rect, Spring};
use crate::events::{Event, EventSender};
use crate::platform::ConnID;

/// Background blur radius when fully open.
const BLUR_RADIUS: f64 = 60.0;
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

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    fn SLSMainConnectionID() -> ConnID;
    fn SLSSetWindowBackgroundBlurRadius(cid: ConnID, wid: i32, radius: i32) -> i32;
}

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

impl WindowServerInfo {
    /// The image of the first window in `list` sitting at the display's
    /// top-left corner.
    fn capture_at(list: &[(i32, Rect)], display: &Rect) -> Option<(CFRetained<CGImage>, Rect)> {
        list.iter()
            .find(|(_, frame)| {
                (frame.x - display.x).abs() < 1.0 && (frame.y - display.y).abs() < 1.0
            })
            .and_then(|(id, frame)| snapshot(*id, true).map(|image| (image, *frame)))
    }
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
        }
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
    best: bool,
    captured: bool,
}

struct RowLayers {
    viewport: Animated,
    label: Animated,
    pan: Animated,
}

struct Panel {
    window: Retained<OverviewPanel>,
    window_number: i32,
    blur: i32,
    tint: Animated,
    container: Animated,
    rows: Vec<RowLayers>,
    cards: Vec<Card>,
}

impl Panel {
    fn layers(&self) -> impl Iterator<Item = &Animated> {
        std::iter::once(&self.tint)
            .chain(std::iter::once(&self.container))
            .chain(
                self.rows
                    .iter()
                    .flat_map(|row| [&row.viewport, &row.label, &row.pan]),
            )
            .chain(self.cards.iter().map(|card| &card.animated))
    }
}

pub struct OverviewManager {
    mtm: MainThreadMarker,
    sender: EventSender,
    panels: Vec<Panel>,
    generation: u64,
}

/// Screen furniture captured once per display and shared by every row.
struct Screen<'a> {
    wallpaper: Option<&'a CGImage>,
    /// Menu bar image and its height.
    menubar: Option<(&'a CGImage, f64)>,
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
        Self {
            mtm,
            sender,
            panels: Vec::new(),
            generation: 0,
        }
    }

    /// Builds the layer trees at progress 0, sitting exactly over the real
    /// screen, and orders the panels in. Captures the wallpaper and the
    /// active rows' windows synchronously first so the hand-off from real
    /// screen to cards is invisible; the rest are filled in later by
    /// [`Self::capture_pending`].
    pub fn build<K>(&mut self, scenes: &[DisplayScene<'_, K>], info: &WindowServerInfo) {
        self.teardown();
        let primary_h = primary_screen_height(self.mtm);
        let start = Instant::now();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for scene in scenes {
            let b = scene.cg_bounds;
            let wallpaper = WindowServerInfo::capture_at(&info.wallpapers, &b);
            let menubar = WindowServerInfo::capture_at(&info.menubars, &b);
            let screen = Screen {
                wallpaper: wallpaper.as_ref().map(|(image, _)| &**image),
                menubar: menubar.as_ref().map(|(image, frame)| (&**image, frame.h)),
            };
            if let Some(panel) = self.build_panel(scene, primary_h, &screen) {
                self.panels.push(panel);
            }
        }
        let mut count = 0;
        for card in self.panels.iter_mut().flat_map(|p| p.cards.iter_mut()) {
            if card.best {
                capture(card);
                count += 1;
            }
        }
        CATransaction::commit();
        debug!(
            "overview: built with {count} active captures in {:?}",
            start.elapsed()
        );

        for panel in &self.panels {
            panel.window.orderFrontRegardless();
            panel.window.makeKeyWindow();
        }
    }

    pub fn has_pending_captures(&self) -> bool {
        self.panels
            .iter()
            .flat_map(|panel| panel.cards.iter())
            .any(|card| !card.captured)
    }

    /// Captures up to `budget` cards still showing a placeholder. Each
    /// capture blocks the main thread for several milliseconds (Core
    /// Animation keeps animating on the render server meanwhile).
    pub fn capture_pending(&mut self, budget: usize) {
        let pending = self
            .panels
            .iter_mut()
            .flat_map(|p| p.cards.iter_mut())
            .filter(|card| !card.captured)
            .take(budget);
        for card in pending {
            capture(card);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_panel<K>(
        &self,
        scene: &DisplayScene<'_, K>,
        primary_h: f64,
        screen: &Screen<'_>,
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
        let tint = Animated::new(CALayer::new(), full, full, (0.0, 1.0));
        tint.layer
            .setBackgroundColor(Some(&color(0.02, 0.02, 0.04, 0.35)));
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
            let viewport = Animated::new(
                CALayer::new(),
                strip.viewport_rest,
                strip.viewport_overview,
                (1.0, 1.0),
            );
            if let Some(image) = screen.wallpaper {
                set_contents(&viewport.layer, image);
                viewport
                    .layer
                    .setContentsGravity(unsafe { kCAGravityResizeAspectFill });
            } else {
                viewport
                    .layer
                    .setBackgroundColor(Some(&color(0.2, 0.22, 0.26, 1.0)));
            }
            viewport.layer.setMasksToBounds(true);
            // The menu bar shrinks with the row's screen. The viewport's
            // bounds are the display's, so it sits at its real place.
            if let Some((image, height)) = screen.menubar {
                let bar = CALayer::new();
                bar.setFrame(CGRect::new(
                    CGPoint::new(0.0, 0.0),
                    CGSize::new(viewport.base.0, height),
                ));
                set_contents(&bar, image);
                bar.setContentsGravity(unsafe { kCAGravityResize });
                viewport.layer.addSublayer(&bar);
            }
            viewport
                .layer
                .setCornerRadius(viewport.open_points(CARD_RADIUS));
            viewport
                .layer
                .setBorderColor(Some(&color(0.45, 0.65, 1.0, 0.9)));
            if strip.active {
                viewport
                    .layer
                    .setBorderWidth(viewport.open_points(RING_WIDTH));
            }
            container.layer.addSublayer(&viewport.layer);
            viewport.set(0.0);

            let label = CATextLayer::new();
            let text = NSString::from_str(&format!("Workspace {}", strip.virtual_index + 1));
            unsafe { label.setString(Some(&text)) };
            label.setFontSize(13.0);
            label.setForegroundColor(Some(&color(0.92, 0.93, 0.95, 1.0)));
            label.setAlignmentMode(unsafe { kCAAlignmentLeft });
            label.setTruncationMode(unsafe { kCATruncationEnd });
            label.setContentsScale(scale);
            let label_rect = strip.label.offset(0.0, 6.0);
            let label = Animated::new(
                Retained::into_super(label),
                label_rect,
                label_rect,
                (0.0, 1.0),
            );
            container.layer.addSublayer(&label.layer);
            label.set(0.0);

            let (pan_rest, pan_open) = pan_frames(size, strip.pan);
            let pan = Animated::new(CALayer::new(), pan_rest, pan_open, (1.0, 1.0));
            container.layer.addSublayer(&pan.layer);
            pan.set(0.0);

            for window in &strip.windows {
                let card = Animated::new(CALayer::new(), window.rest, window.overview, (1.0, 1.0));
                card.layer.setMasksToBounds(true);
                card.layer.setCornerRadius(card.open_points(CARD_RADIUS));
                card.layer
                    .setBorderColor(Some(&color(0.45, 0.65, 1.0, 1.0)));
                if window.focused {
                    card.layer.setBorderWidth(card.open_points(RING_WIDTH));
                }
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
                    captured: false,
                };
                if window.floating {
                    floating.push(entry);
                } else {
                    parent.addSublayer(&entry.animated.layer);
                    entry.animated.set(0.0);
                    tiled.push(entry);
                }
            }
            rows.push(RowLayers {
                viewport,
                label,
                pan,
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
            blur: 0,
            tint,
            container,
            rows,
            cards: tiled,
        })
    }

    fn layers(&self) -> impl Iterator<Item = &Animated> {
        self.panels.iter().flat_map(Panel::layers)
    }

    /// Ramps the `WindowServer` blur behind the panels with progress. Not
    /// animatable on the render server, so the ECS steps it every frame.
    pub fn set_blur(&mut self, progress: f64) {
        #[allow(clippy::cast_possible_truncation)]
        let radius = (BLUR_RADIUS * progress.clamp(0.0, 1.0)).round() as i32;
        for panel in &mut self.panels {
            if panel.blur != radius {
                panel.blur = radius;
                unsafe {
                    SLSSetWindowBackgroundBlurRadius(
                        SLSMainConnectionID(),
                        panel.window_number,
                        radius,
                    );
                }
            }
        }
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
        self.set_blur(p);
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
                row.viewport
                    .morph(strip.viewport_rest, strip.viewport_overview, spring);
                let ring = if strip.active {
                    row.viewport.open_points(RING_WIDTH)
                } else {
                    0.0
                };
                row.viewport.layer.setBorderWidth(ring);
                let label = strip.label.offset(0.0, 6.0);
                row.label.morph(label, label, spring);
                let (rest, open) = pan_frames(layout.size, strip.pan);
                row.pan.morph(rest, open, spring);
            }
            for (card, window) in panel.cards.iter_mut().zip(card_order(layout)) {
                card.animated.morph(window.rest, window.overview, spring);
                let ring = if window.focused {
                    card.animated.open_points(RING_WIDTH)
                } else {
                    0.0
                };
                card.animated.layer.setBorderWidth(ring);
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
        for panel in self.panels.drain(..) {
            panel.window.orderOut(None);
        }
    }
}

fn capture(card: &mut Card) {
    card.captured = true;
    let Some(image) = snapshot(card.window_id, card.best) else {
        warn!("overview: no snapshot for window {}", card.window_id);
        return;
    };
    set_contents(&card.animated.layer, &image);
    card.animated.layer.setBackgroundColor(None);
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
