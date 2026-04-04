use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSCompositingOperation, NSFloatingWindowLevel,
    NSFont, NSGraphicsContext, NSParagraphStyle, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableCopying, NSPoint, NSRect, NSSize, NSString,
};

#[derive(Clone, PartialEq)]
pub struct BorderParams {
    pub color: (f64, f64, f64),
    pub opacity: f64,
    pub width: f64,
    pub radius: f64,
}

/// Parameters for the fullscreen dim + cutout overlay.
#[derive(Clone, PartialEq)]
pub struct DimParams {
    pub opacity: f32,
    pub color: (f64, f64, f64),
    /// The focused window rect to cut out (in Cocoa screen coordinates).
    /// `None` means dim everything (no focused window).
    pub cutout: Option<NSRect>,
    pub border: Option<BorderParams>,
}

// ── DimView: fullscreen dark overlay with a transparent cutout + border ──

#[derive(Debug, Clone)]
struct DimViewIvars {
    opacity: f32,
    dim_r: f64,
    dim_g: f64,
    dim_b: f64,
    // Cutout rect in the view's local coordinates.
    cutout_x: f64,
    cutout_y: f64,
    cutout_w: f64,
    cutout_h: f64,
    has_cutout: bool,
    // Border params (only drawn if has_border is true).
    has_border: bool,
    border_r: f64,
    border_g: f64,
    border_b: f64,
    border_opacity: f64,
    border_width: f64,
    border_radius: f64,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruDimView"]
    #[ivars = DimViewIvars]
    #[derive(Debug)]
    struct DimView;

    impl DimView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let ivars = self.ivars();
            let bounds = self.bounds();

            // Fill the entire view with the dim color.
            let dim_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                ivars.dim_r as CGFloat,
                ivars.dim_g as CGFloat,
                ivars.dim_b as CGFloat,
                CGFloat::from(ivars.opacity),
            );
            dim_color.setFill();
            NSBezierPath::fillRect(bounds);

            if ivars.has_cutout {
                let half = if ivars.has_border { ivars.border_width / 2.0 } else { 0.0 };
                let radius = ivars.border_radius as CGFloat;

                // Expand the cutout by half the border width so the clear hole
                // extends just past the window edge. The border straddles the
                // window edge: outer half visible in the cutout, inner half
                // hidden behind the window.
                let cutout = NSRect::new(
                    NSPoint::new(ivars.cutout_x - half, ivars.cutout_y - half),
                    NSSize::new(ivars.cutout_w + ivars.border_width, ivars.cutout_h + ivars.border_width),
                );

                // Punch a rounded transparent hole using Clear compositing.
                if let Some(ctx) = NSGraphicsContext::currentContext() {
                    ctx.setCompositingOperation(NSCompositingOperation::Clear);
                    let hole = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        cutout, radius, radius,
                    );
                    hole.fill();
                    ctx.setCompositingOperation(NSCompositingOperation::SourceOver);
                }

                // Draw border centered on the window edge — half grows
                // outward (visible in the cutout), half grows inward (behind
                // the window).
                if ivars.has_border {
                    let border_rect = NSRect::new(
                        NSPoint::new(ivars.cutout_x, ivars.cutout_y),
                        NSSize::new(ivars.cutout_w, ivars.cutout_h),
                    );
                    let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        border_rect, radius, radius,
                    );
                    path.setLineWidth(ivars.border_width as CGFloat);
                    let border_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                        ivars.border_r as CGFloat,
                        ivars.border_g as CGFloat,
                        ivars.border_b as CGFloat,
                        ivars.border_opacity as CGFloat,
                    );
                    border_color.setStroke();
                    path.stroke();
                }
            }
        }

        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl DimView {
    fn new(mtm: MainThreadMarker, frame: NSRect, params: &DimParams) -> Retained<Self> {
        let (has_cutout, cx, cy, cw, ch) = params.cutout.map_or((false, 0.0, 0.0, 0.0, 0.0), |r| {
            (true, r.origin.x, r.origin.y, r.size.width, r.size.height)
        });
        let (has_border, br, bg, bb, bo, bw, brad) =
            params
                .border
                .as_ref()
                .map_or((false, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0), |b| {
                    (
                        true, b.color.0, b.color.1, b.color.2, b.opacity, b.width, b.radius,
                    )
                });
        let this = Self::alloc(mtm).set_ivars(DimViewIvars {
            opacity: params.opacity,
            dim_r: params.color.0,
            dim_g: params.color.1,
            dim_b: params.color.2,
            cutout_x: cx,
            cutout_y: cy,
            cutout_w: cw,
            cutout_h: ch,
            has_cutout,
            has_border,
            border_r: br,
            border_g: bg,
            border_b: bb,
            border_opacity: bo,
            border_width: bw,
            border_radius: brad,
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

// ── Coordinate helpers ──────────────────────────────────────────────────

/// Convert an absolute CG screen frame (origin top-left, y-down) to Cocoa
/// screen coordinates (origin bottom-left of primary screen, y-up).
fn cg_abs_to_cocoa(frame: NSRect, primary_screen_height: f64) -> NSRect {
    let cocoa_y = primary_screen_height - frame.origin.y - frame.size.height;
    NSRect::new(NSPoint::new(frame.origin.x, cocoa_y), frame.size)
}

fn primary_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    if screens.is_empty() {
        return 0.0;
    }
    screens.objectAtIndex(0).frame().size.height
}

/// Get the full Cocoa screen rect covering all displays.
fn full_screen_rect(mtm: MainThreadMarker) -> NSRect {
    let screens = NSScreen::screens(mtm);
    if screens.is_empty() {
        return NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
    }
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for screen in &screens {
        let f = screen.frame();
        min_x = min_x.min(f.origin.x);
        min_y = min_y.min(f.origin.y);
        max_x = max_x.max(f.origin.x + f.size.width);
        max_y = max_y.max(f.origin.y + f.size.height);
    }
    NSRect::new(
        NSPoint::new(min_x, min_y),
        NSSize::new(max_x - min_x, max_y - min_y),
    )
}

// ── Overlay window factory ──────────────────────────────────────────────

fn make_overlay_window(mtm: MainThreadMarker, cocoa_frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            cocoa_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setLevel(NSFloatingWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenNone,
    );

    window
}
// ── OverlayManager ──────────────────────────────────────────────────────

pub struct OverlayManager {
    mtm: MainThreadMarker,
    /// Single fullscreen overlay window (dim + cutout + border).
    overlay: Option<(Retained<NSWindow>, DimParams)>,
    hidden: bool,
}

impl OverlayManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            mtm,
            overlay: None,
            hidden: false,
        }
    }

    /// Update the single fullscreen overlay.
    /// `focused_abs_cg` is the focused window rect in absolute CG coords,
    /// or `None` if no window is focused.
    pub fn update(
        &mut self,
        dim_opacity: f32,
        dim_color: (f64, f64, f64),
        focused_abs_cg: Option<NSRect>,
        border: Option<&BorderParams>,
    ) {
        let screen_h = primary_screen_height(self.mtm);
        let screen_rect = full_screen_rect(self.mtm);

        // Convert the focused window rect from absolute CG to Cocoa coords,
        // then to the overlay window's local coordinate system.
        let cutout_local = focused_abs_cg.map(|cg_frame| {
            let cocoa = cg_abs_to_cocoa(cg_frame, screen_h);
            // Convert from screen coords to local (window-relative) coords.
            NSRect::new(
                NSPoint::new(
                    cocoa.origin.x - screen_rect.origin.x,
                    // The view is flipped (isFlipped=true), so y goes top-down.
                    // screen_rect top in Cocoa = screen_rect.origin.y + screen_rect.size.height
                    // We need: local_y = screen_top - cocoa_top
                    (screen_rect.origin.y + screen_rect.size.height)
                        - (cocoa.origin.y + cocoa.size.height),
                ),
                cocoa.size,
            )
        });

        let params = DimParams {
            opacity: dim_opacity,
            color: dim_color,
            cutout: cutout_local,
            border: border.cloned(),
        };

        if let Some((window, stored)) = &mut self.overlay {
            if *stored != params {
                // Recreate the content view with new params.
                let view = DimView::new(self.mtm, screen_rect, &params);
                window.setContentView(Some(&view));
                window.setFrame_display(screen_rect, true);
            }
            if self.hidden {
                window.orderFront(None::<&AnyObject>);
                self.hidden = false;
            }
            *stored = params;
        } else {
            let window = make_overlay_window(self.mtm, screen_rect);
            let view = DimView::new(self.mtm, screen_rect, &params);
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            self.overlay = Some((window, params));
            self.hidden = false;
        }
    }

    pub fn remove_all(&mut self) {
        if let Some((window, _)) = self.overlay.take() {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = false;
    }

    pub fn hide_all(&mut self) {
        if self.hidden {
            return;
        }
        if let Some((window, _)) = &self.overlay {
            window.orderOut(None::<&AnyObject>);
        }
        self.hidden = true;
    }
}

// ── WorkspaceIndicator ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct WorkspaceIndicatorViewIvars {
    opacity: f32,
    number: u32,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruWorkspaceIndicatorView"]
    #[ivars = WorkspaceIndicatorViewIvars]
    #[derive(Debug)]
    struct WorkspaceIndicatorView;

    impl WorkspaceIndicatorView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            let ivars = self.ivars();
            let bounds = self.bounds();

            // 1. Draw semi-transparent bezel (dark gray/black)
            let bezel_color = NSColor::colorWithSRGBRed_green_blue_alpha(
                0.1, 0.1, 0.1,
                CGFloat::from(ivars.opacity * 0.8),
            );
            bezel_color.setFill();
            let radius = 12.0;
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                bounds, radius, radius,
            );
            path.fill();

            // 2. Draw text (Workspace number)
            let text = format!("{}", ivars.number);
            let ns_text = NSString::from_str(&text);

            let font_size = bounds.size.height * 0.6; // Scale font with bezel
            let font = NSFont::systemFontOfSize(font_size);
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, CGFloat::from(ivars.opacity));

            let paragraph_style = unsafe {
                let style = NSParagraphStyle::defaultParagraphStyle().mutableCopy();
                let _: () = msg_send![&style, setAlignment: 1isize]; // Center (NSTextAlignmentCenter = 1)
                style
            };

            // Using manual attribute keys as they might be missing from the crate's high-level API
            let attr_str: Retained<NSAttributedString> = unsafe {
                let font_key = NSString::from_str("NSFont");
                let color_key = NSString::from_str("NSColor");
                let para_key = NSString::from_str("NSParagraphStyle");

                let keys = [&*font_key, &*color_key, &*para_key];
                let objects = [
                    &*font as &AnyObject,
                    &*color as &AnyObject,
                    &*paragraph_style as &AnyObject,
                ];

                let attributes = NSDictionary::from_slices(&keys, &objects);

                // Using raw msg_send as the high-level wrapper might have trait bound issues
                let alloc = NSAttributedString::alloc();
                msg_send![alloc, initWithString: &*ns_text, attributes: &*attributes]
            };

            let text_size = unsafe {
                let size: NSSize = msg_send![&attr_str, size];
                size
            };

            let text_rect = NSRect::new(
                NSPoint::new(
                    bounds.origin.x + (bounds.size.width - text_size.width) / 2.0,
                    bounds.origin.y + (bounds.size.height - text_size.height) / 2.0,
                ),
                text_size
            );

            unsafe {
                let _: () = msg_send![&attr_str, drawInRect: text_rect];
            };
        }
    }
);

impl WorkspaceIndicatorView {
    fn new(mtm: MainThreadMarker, frame: NSRect, number: u32, opacity: f32) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WorkspaceIndicatorViewIvars { opacity, number });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct WorkspaceIndicatorManager {
    mtm: MainThreadMarker,
    window: Option<Retained<NSWindow>>,
}

impl WorkspaceIndicatorManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self { mtm, window: None }
    }

    pub fn show(&mut self, number: u32, opacity: f32, top_right_abs_cg: NSPoint) {
        const INDICATOR_SIZE: f64 = 200.0;
        let size = NSSize::new(INDICATOR_SIZE, INDICATOR_SIZE);
        let padding = 20.0;

        let screen_h = primary_screen_height(self.mtm);
        // top_right_abs_cg is top-left y-down.
        // We want the window to be at (top_right_abs_cg.x - size.width - padding, top_right_abs_cg.y + padding)
        let cocoa_origin_x = top_right_abs_cg.x - size.width - padding;
        let cocoa_origin_y = screen_h - (top_right_abs_cg.y + size.height + padding);

        let frame = NSRect::new(NSPoint::new(cocoa_origin_x, cocoa_origin_y), size);

        if let Some(window) = &self.window {
            let view = WorkspaceIndicatorView::new(
                self.mtm,
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                number,
                opacity,
            );
            window.setContentView(Some(&view));
            window.setFrame_display(frame, true);
            window.orderFront(None::<&AnyObject>);
        } else {
            let window = make_overlay_window(self.mtm, frame);
            // Ensure it's even higher than normal overlays if needed
            window.setLevel(NSFloatingWindowLevel + 1);
            let view = WorkspaceIndicatorView::new(
                self.mtm,
                NSRect::new(NSPoint::new(0.0, 0.0), size),
                number,
                opacity,
            );
            window.setContentView(Some(&view));
            window.orderFront(None::<&AnyObject>);
            self.window = Some(window);
        }
    }

    pub fn remove(&mut self) {
        if let Some(window) = self.window.take() {
            window.orderOut(None::<&AnyObject>);
        }
    }
}
