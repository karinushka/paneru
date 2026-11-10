//! Native projection of the overview: one borderless, non-activating panel
//! per display above everything, holding a Core Animation layer tree.
//!
//! Each animated layer stores its `rest` (closed) and `overview` (open) frame;
//! progress `p` maps to `rest.lerp(overview, p)`. Discrete transitions hand a
//! `CASpringAnimation` to the render server so they stay smooth regardless of
//! how busy paneru's main loop is; gesture scrubbing sets the model values
//! directly with implicit actions disabled.
//!
//! Window contents are static snapshots from `CGWindowListCreateImage` — good
//! enough to prove the transition. Live previews would swap in a
//! `ScreenCaptureKit` stream per card without changing the layer structure.

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
use objc2_core_graphics::{CGColor, CGImage};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CALayer, CAMediaTiming as _, CASpringAnimation, CATextLayer, CATransaction, kCAAlignmentLeft,
    kCAGravityResize, kCATruncationEnd,
};
use tracing::{debug, warn};

use super::{DisplayLayout, Rect, Spring};
use crate::events::{Event, EventSender};

// ── Window snapshots ────────────────────────────────────────────────────

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

fn ns_value_rect(r: CGRect) -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSValue), valueWithRect: r] }
}

fn ns_number(v: f64) -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSNumber), numberWithDouble: v] }
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

/// A layer whose geometry interpolates between two frames, relative to its
/// parent's (possibly also interpolating) frame.
struct Animated {
    layer: Retained<CALayer>,
    rest: Rect,
    overview: Rect,
    /// Opacity at progress 0 and 1.
    opacity: (f64, f64),
}

impl Animated {
    fn frame(&self, p: f64) -> Rect {
        self.rest.lerp(&self.overview, p)
    }

    fn opacity_at(&self, p: f64) -> f64 {
        (self.opacity.0 + (self.opacity.1 - self.opacity.0) * p).clamp(0.0, 1.0)
    }

    fn set(&self, p: f64) {
        let f = self.frame(p);
        self.layer
            .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(f.w, f.h)));
        self.layer.setPosition(centre(&f));
        #[allow(clippy::cast_possible_truncation)]
        self.layer.setOpacity(self.opacity_at(p) as f32);
    }

    fn animate(&self, spring: &Spring) {
        let (a, b) = (self.frame(spring.from), self.frame(spring.to));
        self.set(spring.to);
        self.layer.removeAllAnimations();
        let anims = [
            spring_anim(
                "position",
                &ns_value_point(centre(&a)),
                &ns_value_point(centre(&b)),
                spring,
            ),
            spring_anim(
                "bounds",
                &ns_value_rect(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(a.w, a.h))),
                &ns_value_rect(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(b.w, b.h))),
                spring,
            ),
        ];
        for anim in &anims {
            self.layer.addAnimation_forKey(anim, None);
        }
        if (self.opacity.0 - self.opacity.1).abs() > f64::EPSILON {
            let fade = spring_anim(
                "opacity",
                &ns_number(self.opacity_at(spring.from)),
                &ns_number(self.opacity_at(spring.to)),
                spring,
            );
            self.layer.addAnimation_forKey(&fade, None);
        }
    }
}

// ── Click / key forwarding view ─────────────────────────────────────────

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
            let local = event.locationInWindow();
            let height = self.bounds().size.height;
            let (ox, oy) = self.ivars().origin;
            let point = CGPoint::new(ox + local.x, oy + (height - local.y));
            _ = self.ivars().sender.send(Event::OverviewClick { point });
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

struct Panel {
    window: Retained<OverviewPanel>,
    backdrop: Animated,
    /// Viewport backgrounds, labels and ribbon clips, drawn in that order.
    chrome: Vec<Animated>,
    cards: Vec<Card>,
}

pub struct OverviewManager {
    mtm: MainThreadMarker,
    sender: EventSender,
    panels: Vec<Panel>,
    generation: u64,
}

/// One display to show: its absolute CG bounds and the computed layout.
pub struct DisplayScene<'a, K> {
    pub cg_bounds: Rect,
    pub layout: &'a DisplayLayout<K>,
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
    /// windows, and orders the panels in. Captures the active rows' windows
    /// synchronously first so the hand-off from real window to card is
    /// invisible; the rest are filled in by [`Self::capture_pending`].
    pub fn build<K>(&mut self, scenes: &[DisplayScene<'_, K>]) {
        self.teardown();
        let primary_h = primary_screen_height(self.mtm);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for scene in scenes {
            if let Some(panel) = self.build_panel(scene, primary_h) {
                self.panels.push(panel);
            }
        }
        CATransaction::commit();

        let start = Instant::now();
        let mut count = 0;
        for card in self.panels.iter_mut().flat_map(|p| p.cards.iter_mut()) {
            if card.best {
                capture(card);
                count += 1;
            }
        }
        debug!(
            "overview: captured {count} active windows in {:?}",
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

    /// Captures every card still showing a placeholder. Spread over frames
    /// by the caller via `budget`, since each capture blocks the main thread
    /// (Core Animation keeps animating on the render server meanwhile).
    pub fn capture_pending(&mut self, budget: usize) -> bool {
        let mut done = 0;
        for card in self.panels.iter_mut().flat_map(|p| p.cards.iter_mut()) {
            if !card.captured {
                if done == budget {
                    return true;
                }
                capture(card);
                done += 1;
            }
        }
        false
    }

    #[allow(clippy::too_many_lines)]
    fn build_panel<K>(&self, scene: &DisplayScene<'_, K>, primary_h: f64) -> Option<Panel> {
        let mtm = self.mtm;
        let b = scene.cg_bounds;
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

        // Y-down container so every rect maps 1:1.
        let container = CALayer::new();
        container.setGeometryFlipped(true);
        container.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(b.w, b.h)));
        root.addSublayer(&container);

        let full = Rect::new(0.0, 0.0, b.w, b.h);
        let backdrop = Animated {
            layer: CALayer::new(),
            rest: full,
            overview: full,
            opacity: (0.0, 1.0),
        };
        backdrop
            .layer
            .setBackgroundColor(Some(&color(0.07, 0.07, 0.09, 0.92)));
        container.addSublayer(&backdrop.layer);
        backdrop.set(0.0);

        let mut chrome = Vec::new();
        let mut cards = Vec::new();
        for strip in &scene.layout.strips {
            let viewport = Animated {
                layer: CALayer::new(),
                rest: strip.viewport_rest,
                overview: strip.viewport_overview,
                opacity: (0.0, 1.0),
            };
            viewport
                .layer
                .setBackgroundColor(Some(&color(0.2, 0.22, 0.26, 1.0)));
            viewport.layer.setCornerRadius(10.0);
            if strip.active {
                viewport.layer.setBorderWidth(2.0);
                viewport
                    .layer
                    .setBorderColor(Some(&color(0.45, 0.65, 1.0, 0.9)));
            }
            container.addSublayer(&viewport.layer);
            viewport.set(0.0);

            let label = CATextLayer::new();
            let text = NSString::from_str(&format!("Workspace {}", strip.virtual_index + 1));
            unsafe { label.setString(Some(&text)) };
            label.setFontSize(13.0);
            label.setForegroundColor(Some(&color(0.85, 0.87, 0.9, 1.0)));
            label.setAlignmentMode(unsafe { kCAAlignmentLeft });
            label.setTruncationMode(unsafe { kCATruncationEnd });
            label.setContentsScale(scale);
            let label_rect = strip.label.offset(0.0, 6.0);
            let label = Animated {
                layer: Retained::into_super(label),
                rest: label_rect,
                overview: label_rect,
                opacity: (0.0, 1.0),
            };
            container.addSublayer(&label.layer);
            label.set(0.0);

            // Ribbon clip: full display (row-offset) at rest, the padded
            // strip band when open. Cards are positioned relative to it, and
            // since both lerp with the same factor the composition is exact.
            let clip = Animated {
                layer: CALayer::new(),
                rest: strip.viewport_rest,
                overview: strip.ribbon,
                opacity: (1.0, 1.0),
            };
            clip.layer.setMasksToBounds(true);
            container.addSublayer(&clip.layer);
            clip.set(0.0);

            for window in &strip.windows {
                let rel = |r: Rect, parent: Rect| r.offset(-parent.x, -parent.y);
                let card = Animated {
                    layer: CALayer::new(),
                    rest: rel(window.rest, clip.rest),
                    overview: rel(window.overview, clip.overview),
                    opacity: (1.0, 1.0),
                };
                card.layer.setCornerRadius(8.0);
                card.layer.setMasksToBounds(true);
                card.layer.setContentsGravity(unsafe { kCAGravityResize });
                card.layer.setContentsScale(scale);
                if !strip.active {
                    card.layer
                        .setBackgroundColor(Some(&color(0.3, 0.32, 0.36, 1.0)));
                }
                clip.layer.addSublayer(&card.layer);
                card.set(0.0);
                cards.push(Card {
                    animated: card,
                    window_id: window.window_id,
                    best: strip.active,
                    captured: false,
                });
            }
            chrome.push(viewport);
            chrome.push(label);
            chrome.push(clip);
        }

        Some(Panel {
            window,
            backdrop,
            chrome,
            cards,
        })
    }

    fn layers(&self) -> impl Iterator<Item = &Animated> {
        self.panels.iter().flat_map(|panel| {
            std::iter::once(&panel.backdrop)
                .chain(panel.chrome.iter())
                .chain(panel.cards.iter().map(|card| &card.animated))
        })
    }

    /// Jumps every layer to progress `p` (gesture scrubbing).
    pub fn set_progress(&self, p: f64) {
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

    /// Re-targets the rest frames (e.g. after picking another row) without
    /// moving anything yet.
    pub fn retarget<K>(&mut self, layouts: &[&DisplayLayout<K>]) {
        for (panel, layout) in self.panels.iter_mut().zip(layouts) {
            // Chrome is pushed as (viewport, label, clip) per strip.
            for (strip, trio) in layout.strips.iter().zip(panel.chrome.chunks_mut(3)) {
                trio[0].rest = strip.viewport_rest;
                trio[2].rest = strip.viewport_rest;
            }
            let rests = layout.strips.iter().flat_map(|strip| {
                strip.windows.iter().map(move |w| {
                    w.rest
                        .offset(-strip.viewport_rest.x, -strip.viewport_rest.y)
                })
            });
            for (card, rest) in panel.cards.iter_mut().zip(rests) {
                card.animated.rest = rest;
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
    // SAFETY: a CGImage is a CF object, toll-free usable as `id` contents.
    let object = unsafe { &*(&raw const *image).cast::<AnyObject>() };
    unsafe { card.animated.layer.setContents(Some(object)) };
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
