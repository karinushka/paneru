//! Core Animation layer primitives and card/row label builders for the
//! overview overlay panels.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::NSFont;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage};
use objc2_foundation::NSString;
use objc2_quartz_core::{
    CABasicAnimation, CACurrentMediaTime, CALayer, CAMediaTiming as _, CASpringAnimation,
    CATextLayer, kCAAlignmentCenter, kCAAlignmentLeft, kCAFillModeForwards, kCAGravityResizeAspect,
    kCATruncationEnd,
};

use super::{Rect, Spring};

/// Corner radius and focus ring width of a card when fully open, in points.
pub(super) const CARD_RADIUS: f64 = 10.0;
const RING_WIDTH: f64 = 2.5;

/// Without previews: a card's app icon size and title font size when fully
/// open, in points.
pub(super) const ICON_SIZE: f64 = 56.0;
const TITLE_FONT_SIZE: f64 = 12.0;

/// Size of a row's label text, in points.
const LABEL_FONT_SIZE: f64 = 15.0;

pub(super) const FADE_KEY: &str = "paneru.fade";

/// How long the overlay takes to fade as a close lands.
const FADE_SECS: f64 = 0.04;

/// A row's workspace name, in `rect` (overview points): it stays put
/// while the overview opens and only fades in.
pub(super) fn row_label(virtual_index: u32, rect: &Rect, scale: f64) -> Animated {
    let text = CATextLayer::new();
    let name = NSString::from_str(&format!(
        "Workspace {}",
        crate::menubar::virtual_workspace_label(virtual_index)
    ));
    unsafe { text.setString(Some(&name)) };
    let font = NSFont::boldSystemFontOfSize(LABEL_FONT_SIZE);
    // SAFETY: NSFont is toll-free bridged to CTFont, a CF object.
    let font = unsafe { &*Retained::as_ptr(&font).cast::<objc2_core_foundation::CFType>() };
    unsafe { text.setFont(Some(font)) };
    text.setFontSize(LABEL_FONT_SIZE);
    text.setForegroundColor(Some(&color(1.0, 1.0, 1.0, 0.85)));
    text.setAlignmentMode(unsafe { kCAAlignmentLeft });
    text.setTruncationMode(unsafe { kCATruncationEnd });
    text.setContentsScale(scale);
    // Sit the text on the bottom of the band, just above the strip.
    let h = (LABEL_FONT_SIZE * 1.3).min(rect.h);
    let rect = Rect::new(rect.x + 4.0, rect.y + rect.h - h - 2.0, rect.w - 8.0, h);
    Animated::new(Retained::into_super(text), rect, rect, (0.0, 1.0))
}

/// A card without a preview: a plain tile with the app icon and the window
/// title in the middle. Sized in `card`'s bounds space (the window's real
/// size, scaled down by the card's transform), so it reads the same on
/// every card once open.
pub(super) fn add_tile(
    content: &CALayer,
    card: &Animated,
    title: Option<&str>,
    icon: Option<&CGImage>,
    scale: f64,
) {
    content.setBackgroundColor(Some(&color(0.22, 0.24, 0.28, 1.0)));
    let (w, h) = card.base;
    let icon_size = card.open_points(ICON_SIZE).min(w).min(h);
    let font_size = card.open_points(TITLE_FONT_SIZE);
    let gap = card.open_points(8.0);
    let text_h = font_size * 1.4;
    let top = (h - icon_size - gap - text_h) / 2.0;
    if let Some(image) = icon {
        let layer = CALayer::new();
        layer.setFrame(CGRect::new(
            CGPoint::new((w - icon_size) / 2.0, top.max(0.0)),
            CGSize::new(icon_size, icon_size),
        ));
        layer.setContentsGravity(unsafe { kCAGravityResizeAspect });
        set_contents(&layer, image);
        content.addSublayer(&layer);
    }
    let Some(title) = title.filter(|t| !t.is_empty()) else {
        return;
    };
    let text = CATextLayer::new();
    unsafe { text.setString(Some(&NSString::from_str(title))) };
    let font = NSFont::systemFontOfSize(TITLE_FONT_SIZE);
    // SAFETY: NSFont is toll-free bridged to CTFont, a CF object.
    let font = unsafe { &*Retained::as_ptr(&font).cast::<objc2_core_foundation::CFType>() };
    unsafe { text.setFont(Some(font)) };
    text.setFontSize(font_size);
    text.setForegroundColor(Some(&color(1.0, 1.0, 1.0, 0.85)));
    text.setAlignmentMode(unsafe { kCAAlignmentCenter });
    text.setTruncationMode(unsafe { kCATruncationEnd });
    text.setContentsScale(scale);
    let margin = card.open_points(12.0);
    text.setFrame(CGRect::new(
        CGPoint::new(margin, (top + icon_size + gap).min(h - text_h)),
        CGSize::new((w - 2.0 * margin).max(1.0), text_h),
    ));
    content.addSublayer(&text);
}

pub(super) fn color(r: f64, g: f64, b: f64, a: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(r, g, b, a)
}

fn centre(r: &Rect) -> CGPoint {
    CGPoint::new(r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn ns_value_point(p: CGPoint) -> Retained<AnyObject> {
    unsafe { msg_send![class!(NSValue), valueWithPoint: p] }
}

pub(super) fn ns_number(v: f64) -> Retained<AnyObject> {
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
    anim.setDuration(spring.visible_duration());
    anim
}

/// Fades `layer` out over the last [`FADE_SECS`] of a close lasting `secs`,
/// on the render server: the real windows show at the moment the close
/// lands even when the main thread is too busy to take the overlay down.
pub(super) fn fade_out_at_end(layer: &CALayer, secs: f64) {
    let fade = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
    unsafe {
        fade.setFromValue(Some(&ns_number(1.0)));
        fade.setToValue(Some(&ns_number(0.0)));
    }
    fade.setBeginTime(CACurrentMediaTime() + (secs - FADE_SECS).max(0.0));
    fade.setDuration(FADE_SECS.min(secs));
    fade.setFillMode(unsafe { kCAFillModeForwards });
    fade.setRemovedOnCompletion(false);
    layer.addAnimation_forKey(&fade, Some(&NSString::from_str(FADE_KEY)));
}

pub(super) fn set_contents(layer: &CALayer, image: &CGImage) {
    // SAFETY: a CGImage is a CF object, usable as `id` layer contents.
    let object = unsafe { &*(&raw const *image).cast::<AnyObject>() };
    unsafe { layer.setContents(Some(object)) };
}

/// A layer whose frame interpolates between `rest` (progress 0) and
/// `overview` (progress 1). Bounds stay at `base`; the frame is produced
/// with `position` and `transform.scale`.
pub(super) struct Animated {
    pub(super) layer: Retained<CALayer>,
    pub(super) rest: Rect,
    pub(super) overview: Rect,
    /// Opacity at progress 0 and 1.
    opacity: (f64, f64),
    pub(super) base: (f64, f64),
    /// Highlight ring width in bounds space when fully open; it grows in with
    /// progress so nothing outlines the real screen at the start.
    ring: f64,
    /// Corner radius in bounds space when fully open; grows in like `ring`
    /// so the full-size frame at progress 0 has square corners.
    pub(super) corner: f64,
}

impl Animated {
    pub(super) fn new(
        layer: Retained<CALayer>,
        rest: Rect,
        overview: Rect,
        opacity: (f64, f64),
    ) -> Self {
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

    pub(super) fn set_ring(&mut self, on: bool) {
        self.ring = if on {
            self.open_points(RING_WIDTH)
        } else {
            0.0
        };
    }

    /// Bounds-space length that renders as `points` when fully open.
    pub(super) fn open_points(&self, points: f64) -> f64 {
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

    pub(super) fn set(&self, p: f64) {
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
    pub(super) fn animate(&self, spring: &Spring) {
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
    pub(super) fn morph(&mut self, rest: Rect, overview: Rect, spring: &Spring) {
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
