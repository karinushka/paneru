//! Core Animation layer primitives and card/row label builders for the
//! overview overlay panels.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::NSFont;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGImage};
use objc2_foundation::{NSString, ns_string};
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

pub(super) fn fade_key() -> &'static NSString {
    ns_string!("paneru.fade")
}

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

fn set_key_path(layer: &CALayer, key: &NSString, value: f64) {
    let value = ns_number(value);
    unsafe {
        let _: () = msg_send![layer, setValue: &*value, forKeyPath: key];
    }
}

fn spring_anim(
    key: &NSString,
    from: &AnyObject,
    to: &AnyObject,
    spring: &Spring,
    duration: f64,
) -> Retained<CASpringAnimation> {
    let anim = CASpringAnimation::animationWithKeyPath(Some(key));
    anim.setMass(1.0);
    anim.setStiffness(spring.stiffness());
    anim.setDamping(spring.damping());
    anim.setInitialVelocity(spring.normalized_velocity());
    unsafe {
        anim.setFromValue(Some(from));
        anim.setToValue(Some(to));
    }
    anim.setDuration(duration);
    anim
}

/// Fades `layer` out over the last [`FADE_SECS`] of a close lasting `secs`,
/// on the render server: the real windows show at the moment the close
/// lands even when the main thread is too busy to take the overlay down.
pub(super) fn fade_out_at_end(layer: &CALayer, secs: f64) {
    let fade = CABasicAnimation::animationWithKeyPath(Some(ns_string!("opacity")));
    unsafe {
        fade.setFromValue(Some(&ns_number(1.0)));
        fade.setToValue(Some(&ns_number(0.0)));
    }
    fade.setBeginTime(CACurrentMediaTime() + (secs - FADE_SECS).max(0.0));
    fade.setDuration(FADE_SECS.min(secs));
    fade.setFillMode(unsafe { kCAFillModeForwards });
    fade.setRemovedOnCompletion(false);
    layer.addAnimation_forKey(&fade, Some(fade_key()));
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
    /// Monotonic counter giving each additive morph animation a unique key on
    /// `layer` so rapid consecutive morphs compose rather than overwrite.
    morph_seq: u64,
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
            morph_seq: 0,
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
        set_key_path(
            &self.layer,
            ns_string!("transform.scale.x"),
            f.w / self.base.0,
        );
        set_key_path(
            &self.layer,
            ns_string!("transform.scale.y"),
            f.h / self.base.1,
        );
        #[allow(clippy::cast_possible_truncation)]
        self.layer.setOpacity(opacity as f32);
    }

    pub(super) fn set(&self, p: f64) {
        self.apply(&self.frame(p), self.opacity_at(p));
        let p = p.clamp(0.0, 1.0);
        self.layer.setBorderWidth(self.ring * p);
        self.layer.setCornerRadius(self.corner * p);
    }

    fn spring_between(
        &self,
        a: &Rect,
        b: &Rect,
        opacity: (f64, f64),
        spring: &Spring,
        duration: f64,
    ) {
        self.layer.removeAllAnimations();
        self.apply(b, opacity.1);
        let anims = [
            spring_anim(
                ns_string!("position"),
                &ns_value_point(centre(a)),
                &ns_value_point(centre(b)),
                spring,
                duration,
            ),
            spring_anim(
                ns_string!("transform.scale.x"),
                &ns_number(a.w / self.base.0),
                &ns_number(b.w / self.base.0),
                spring,
                duration,
            ),
            spring_anim(
                ns_string!("transform.scale.y"),
                &ns_number(a.h / self.base.1),
                &ns_number(b.h / self.base.1),
                spring,
                duration,
            ),
        ];
        for anim in &anims {
            let key = anim.keyPath();
            self.layer.addAnimation_forKey(anim, key.as_deref());
        }
        if (opacity.0 - opacity.1).abs() > f64::EPSILON {
            let fade = spring_anim(
                ns_string!("opacity"),
                &ns_number(opacity.0),
                &ns_number(opacity.1),
                spring,
                duration,
            );
            let key = fade.keyPath();
            self.layer.addAnimation_forKey(&fade, key.as_deref());
        }
    }

    /// Springs from progress `spring.from` to `spring.to` over `duration` seconds.
    pub(super) fn animate(&self, spring: &Spring, duration: f64) {
        let (a, b) = (self.frame(spring.from), self.frame(spring.to));
        let opacity = (self.opacity_at(spring.from), self.opacity_at(spring.to));
        self.spring_between(&a, &b, opacity, spring, duration);
        for (key, full) in [
            (ns_string!("borderWidth"), self.ring),
            (ns_string!("cornerRadius"), self.corner),
        ] {
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
                duration,
            );
            self.layer.addAnimation_forKey(&grow, Some(key));
        }
    }

    /// Moves the open (and rest) frame to new targets, springing from where
    /// it is now. Assumes the overview is fully open.
    ///
    /// Layers additive `CASpringAnimation`s (`delta → 0`) onto the updated
    /// model target instead of calling `removeAllAnimations()`, so repeated
    /// focus steps while an earlier slide is still in flight keep their live
    /// position and velocity rather than snapping to the previous target.
    pub(super) fn morph(&mut self, rest: Rect, overview: Rect, spring: &Spring, duration: f64) {
        let from = self.overview;
        self.rest = rest;
        self.overview = overview;
        if from != overview {
            self.apply(&overview, self.opacity_at(1.0));
            self.morph_seq += 1;
            let seq = self.morph_seq;
            let (ca, cb) = (centre(&from), centre(&overview));
            let (dx, dy) = (ca.x - cb.x, ca.y - cb.y);
            if dx.abs() > f64::EPSILON || dy.abs() > f64::EPSILON {
                let anim = spring_anim(
                    ns_string!("position"),
                    &ns_value_point(CGPoint::new(dx, dy)),
                    &ns_value_point(CGPoint::new(0.0, 0.0)),
                    spring,
                    duration,
                );
                anim.setAdditive(true);
                let key = NSString::from_str(&format!("paneru.morph.{seq:08}.position"));
                self.layer.addAnimation_forKey(&anim, Some(&key));
            }
            for (key_path, axis, delta) in [
                (
                    ns_string!("transform.scale.x"),
                    "x",
                    (from.w - overview.w) / self.base.0,
                ),
                (
                    ns_string!("transform.scale.y"),
                    "y",
                    (from.h - overview.h) / self.base.1,
                ),
            ] {
                if delta.abs() > f64::EPSILON {
                    let anim = spring_anim(
                        key_path,
                        &ns_number(delta),
                        &ns_number(0.0),
                        spring,
                        duration,
                    );
                    anim.setAdditive(true);
                    let key = NSString::from_str(&format!("paneru.morph.{seq:08}.scale.{axis}"));
                    self.layer.addAnimation_forKey(&anim, Some(&key));
                }
            }
        }
        self.layer.setBorderWidth(self.ring);
        self.layer.setCornerRadius(self.corner);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::overview::SPRING_RESPONSE;

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    struct AnimIdentity {
        from_x: u64,
        from_y: u64,
        to_x: u64,
        to_y: u64,
    }

    /// Tracks when each `CAAnimation` currently attached to a `CALayer` began,
    /// mirroring `CARenderServer` stamping `beginTime` on `addAnimation:forKey:`.
    #[derive(Default)]
    struct AnimationClock {
        started: HashMap<String, (AnimIdentity, f64)>,
    }

    impl AnimationClock {
        fn sync(&mut self, layer: &CALayer, now: f64) {
            let mut live = HashMap::new();
            let Some(keys) = layer.animationKeys() else {
                self.started.clear();
                return;
            };
            for key in &keys {
                let Some(anim) = (unsafe { layer.animationForKey(&key) }) else {
                    continue;
                };
                let Ok(spring) = anim.downcast::<CASpringAnimation>() else {
                    continue;
                };
                let id = anim_identity(&spring);
                let name = key.to_string();
                let start = match self.started.get(&name) {
                    Some((prev_id, t0)) if *prev_id == id => *t0,
                    _ => now,
                };
                live.insert(name, (id, start));
            }
            self.started = live;
        }

        /// Evaluates the composite presentation `(frame_x, vx, width)` of `card`
        /// at `now` using Core Animation's render-server composition rules
        /// (model value + active animations in key order).
        fn sample(&self, card: &Animated, now: f64) -> (f64, f64, f64) {
            let mut cx = card.layer.position().x;
            let mut vx = 0.0;
            let sx_key = NSString::from_str("transform.scale.x");
            let model_sx: f64 = unsafe {
                let num: Retained<AnyObject> = msg_send![&card.layer, valueForKeyPath: &*sx_key];
                msg_send![&*num, doubleValue]
            };
            let mut sx = model_sx;

            if let Some(keys) = card.layer.animationKeys() {
                let mut names: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
                names.sort();
                for name in names {
                    let Some((_, start)) = self.started.get(&name) else {
                        continue;
                    };
                    let dt = now - start;
                    let Some(anim) =
                        (unsafe { card.layer.animationForKey(&NSString::from_str(&name)) })
                    else {
                        continue;
                    };
                    let Ok(spring_anim) = anim.downcast::<CASpringAnimation>() else {
                        continue;
                    };
                    if dt < 0.0 || dt >= spring_anim.duration() {
                        continue;
                    }
                    let Some(key_path) = spring_anim.keyPath() else {
                        continue;
                    };
                    let omega = spring_anim.stiffness().sqrt();
                    let Some((from, to)) = endpoints(&spring_anim) else {
                        continue;
                    };
                    let eval = |a: f64, b: f64| -> (f64, f64) {
                        let s = Spring {
                            from: a,
                            to: b,
                            velocity: spring_anim.initialVelocity() * (b - a),
                            omega,
                        };
                        (s.value(dt), s.velocity_at(dt))
                    };
                    match key_path.to_string().as_str() {
                        "position" => {
                            let (px, pvx) = eval(from.0, to.0);
                            if spring_anim.isAdditive() {
                                cx += px;
                                vx += pvx;
                            } else {
                                cx = px;
                                vx = pvx;
                            }
                        }
                        "transform.scale.x" => {
                            let (psx, _) = eval(from.0, to.0);
                            if spring_anim.isAdditive() {
                                sx += psx;
                            } else {
                                sx = psx;
                            }
                        }
                        _ => {}
                    }
                }
            }
            let width = card.base.0 * sx;
            let frame_x = cx - width / 2.0;
            (frame_x, vx, width)
        }
    }

    fn endpoints(anim: &CASpringAnimation) -> Option<((f64, f64), (f64, f64))> {
        let key_path = anim.keyPath()?.to_string();
        let from = anim.fromValue()?;
        let to = anim.toValue()?;
        if key_path == "position" {
            let fp: CGPoint = unsafe { msg_send![&*from, pointValue] };
            let tp: CGPoint = unsafe { msg_send![&*to, pointValue] };
            Some(((fp.x, fp.y), (tp.x, tp.y)))
        } else {
            let fv: f64 = unsafe { msg_send![&*from, doubleValue] };
            let tv: f64 = unsafe { msg_send![&*to, doubleValue] };
            Some(((fv, 0.0), (tv, 0.0)))
        }
    }

    fn anim_identity(anim: &CASpringAnimation) -> AnimIdentity {
        let ((fx, fy), (tx, ty)) = endpoints(anim).unwrap_or_default();
        AnimIdentity {
            from_x: fx.to_bits(),
            from_y: fy.to_bits(),
            to_x: tx.to_bits(),
            to_y: ty.to_bits(),
        }
    }

    #[test]
    fn repeated_morphs_preserve_in_flight_position_and_velocity() {
        use objc2_quartz_core::CATransaction;

        // Hold an explicit outer `CATransaction` open for the test's duration:
        // on a headless worker thread (no `NSWindow` / `CAContext`), committing
        // or background-flushing the root transaction immediately discards
        // `removedOnCompletion` animations because there is no render context.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        scopeguard::defer! {
            CATransaction::commit();
        }

        let rest = Rect::new(0.0, 0.0, 1200.0, 800.0);
        let open_0 = Rect::new(1000.0, 100.0, 600.0, 400.0);
        let mut card = Animated::new(CALayer::new(), rest, open_0, (1.0, 1.0));
        // Freeze the test layer's media clock so OS scheduler preemption >363ms
        // under heavy parallel test load never expires animations mid-assertion.
        card.layer.setSpeed(0.0);
        card.set(1.0);

        let spring = Spring::new(0.0, 1.0, 0.0, SPRING_RESPONSE);
        let duration = spring.visible_duration();
        let mut clock = AnimationClock::default();
        let step = |card: &mut Animated, clock: &mut AnimationClock, target: Rect, now: f64| {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            card.morph(rest, target, &spring, duration);
            CATransaction::commit();
            clock.sync(&card.layer, now);
        };

        // 1st focus change at t = 0ms: strip slides left by 400pt (1000 -> 600).
        let open_1 = Rect::new(600.0, 100.0, 600.0, 400.0);
        step(&mut card, &mut clock, open_1, 0.0);

        let (x_start, vx_start, _) = clock.sample(&card, 0.0);
        assert!(
            (x_start - 1000.0).abs() < 1e-6 && vx_start.abs() < 1e-6,
            "first morph must start smoothly from 1000.0, got x={x_start}, vx={vx_start}"
        );

        // 60ms into the slide, the card is mid-flight with substantial leftward velocity.
        let (x_mid_1, vx_mid_1, _) = clock.sample(&card, 0.06);
        assert!(
            x_mid_1 > 750.0 && x_mid_1 < 900.0 && vx_mid_1 < -2000.0,
            "card should be mid-slide at t=60ms, got x={x_mid_1}, vx={vx_mid_1}"
        );

        // 2nd focus change in the same direction at t = 60ms (600 -> 200):
        // must NOT snap forward to 600.0 or zero out velocity!
        let open_2 = Rect::new(200.0, 100.0, 600.0, 400.0);
        step(&mut card, &mut clock, open_2, 0.06);

        let (x_after_2, vx_after_2, _) = clock.sample(&card, 0.06);
        assert!(
            (x_after_2 - x_mid_1).abs() < 1e-6,
            "2nd morph jumped position instantaneously from {x_mid_1:.2} to {x_after_2:.2}"
        );
        assert!(
            (vx_after_2 - vx_mid_1).abs() < 1e-6,
            "2nd morph reset velocity from {vx_mid_1:.1} to {vx_after_2:.1}"
        );

        // 3rd focus change in the same direction at t = 120ms (200 -> -200)
        // plus a width change (600 -> 450) to verify scale continuity too.
        let (x_mid_2, vx_mid_2, w_mid_2) = clock.sample(&card, 0.12);
        let open_3 = Rect::new(-200.0, 100.0, 450.0, 300.0);
        step(&mut card, &mut clock, open_3, 0.12);

        let (x_after_3, vx_after_3, w_after_3) = clock.sample(&card, 0.12);
        assert!(
            (x_after_3 - x_mid_2).abs() < 1e-6,
            "3rd morph jumped position instantaneously from {x_mid_2:.2} to {x_after_3:.2}"
        );
        assert!(
            (vx_after_3 - vx_mid_2).abs() < 1e-6,
            "3rd morph reset velocity from {vx_mid_2:.1} to {vx_after_3:.1}"
        );
        assert!(
            (w_after_3 - w_mid_2).abs() < 1e-6,
            "3rd morph jumped width from {w_mid_2:.2} to {w_after_3:.2}"
        );

        // Once all additive springs finish, the card rests at the final target.
        let settled_t = 0.12 + spring.visible_duration() + 0.01;
        let (x_end, vx_end, w_end) = clock.sample(&card, settled_t);
        assert!(
            (x_end - (-200.0)).abs() < 1e-6 && vx_end.abs() < 1e-6 && (w_end - 450.0).abs() < 1e-6,
            "settled card should rest at (-200, w=450), got x={x_end}, vx={vx_end}, w={w_end}"
        );

        // Reversing direction mid-flight (e.g. focus east then focus west 50ms
        // later) also preserves position and velocity without snapping.
        let open_4 = Rect::new(200.0, 100.0, 450.0, 300.0);
        step(&mut card, &mut clock, open_4, settled_t);
        let rev_t = settled_t + 0.05;
        let (x_before_rev, vx_before_rev, _) = clock.sample(&card, rev_t);
        step(&mut card, &mut clock, open_3, rev_t);
        let (x_after_rev, vx_after_rev, _) = clock.sample(&card, rev_t);
        assert!(
            (x_after_rev - x_before_rev).abs() < 1e-6
                && (vx_after_rev - vx_before_rev).abs() < 1e-6,
            "direction reversal jumped from ({x_before_rev:.2}, {vx_before_rev:.1}) to ({x_after_rev:.2}, {vx_after_rev:.1})"
        );
    }
}
