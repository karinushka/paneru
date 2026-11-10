//! Niri-style overview: every virtual workspace row of a display shown as a
//! scaled-down horizontal strip, stacked vertically.
//!
//! This module is the pure half — geometry, the transition spring and hit
//! testing — so it stays unit-testable. The native half (`view`) projects the
//! result into Core Animation layers on a per-display overlay panel; the real
//! windows are never moved or scaled while the overview is open.
//!
//! Every rect here is display-local and Y-down (origin at the display's
//! top-left corner), matching CG screen coordinates minus the display origin.

pub mod view;

/// Display-local, Y-down rectangle in points.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn offset(&self, dx: f64, dy: f64) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn lerp(&self, other: &Self, t: f64) -> Self {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        Self::new(
            mix(self.x, other.x),
            mix(self.y, other.y),
            mix(self.w, other.w),
            mix(self.h, other.h),
        )
    }
}

// ── Inputs ──────────────────────────────────────────────────────────────

/// One window as the ECS sees it.
#[derive(Clone, Debug)]
pub struct WindowInput<K> {
    pub key: K,
    pub window_id: i32,
    /// Where the window would sit if its strip were the active one, in
    /// display-local coordinates. Columns scrolled off the viewport keep
    /// their unclamped positions so the strip reads as a ribbon.
    pub layout_frame: Rect,
    /// Where the window really is right now, for windows of the active strip
    /// (so the transition starts exactly on top of them). `None` for windows
    /// parked off-screen (hidden rows, inactive tabs).
    pub real_frame: Option<Rect>,
}

#[derive(Clone, Debug)]
pub struct StripInput<K> {
    pub virtual_index: u32,
    pub active: bool,
    pub windows: Vec<WindowInput<K>>,
}

// ── Layout ──────────────────────────────────────────────────────────────

/// Spacing constants, before `zoom` is applied.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub top_padding: f64,
    pub bottom_padding: f64,
    pub label_height: f64,
    pub section_gap: f64,
    pub ribbon_padding: f64,
    /// How many whole rows (label + strip) fit the display height at zoom 1.
    pub visible_rows: f64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            top_padding: 48.0,
            bottom_padding: 40.0,
            label_height: 28.0,
            section_gap: 20.0,
            ribbon_padding: 24.0,
            visible_rows: 2.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WindowPlacement<K> {
    pub key: K,
    pub window_id: i32,
    /// Frame while closed (progress 0).
    pub rest: Rect,
    /// Frame while open (progress 1).
    pub overview: Rect,
}

#[derive(Clone, Debug)]
pub struct StripPlacement<K> {
    pub virtual_index: u32,
    pub active: bool,
    /// Label above the strip (fades in, no motion).
    pub label: Rect,
    /// The scaled-down display viewport ("the screen" of this row).
    pub viewport_rest: Rect,
    pub viewport_overview: Rect,
    /// Horizontal band the strip's windows are clipped to while open: the
    /// full display width minus padding, so off-viewport columns peek out.
    pub ribbon: Rect,
    pub windows: Vec<WindowPlacement<K>>,
}

#[derive(Clone, Debug)]
pub struct DisplayLayout<K> {
    pub size: (f64, f64),
    pub scale: f64,
    pub strips: Vec<StripPlacement<K>>,
}

/// Lays out one display's rows as niri does: a vertical stack of strips,
/// each a scaled copy of the full display, with the active strip kept in
/// view. Rows other than the active one rest one display height above or
/// below it, so on open they slide in from off-screen while shrinking.
#[allow(clippy::cast_precision_loss, reason = "row counts are tiny")]
pub fn layout_display<K: Clone>(
    size: (f64, f64),
    strips: &[StripInput<K>],
    zoom: f64,
    metrics: &Metrics,
) -> DisplayLayout<K> {
    let (width, height) = size;
    let zoom = zoom.clamp(0.5, 1.5);
    let top = metrics.top_padding * zoom;
    let bottom = metrics.bottom_padding * zoom;
    let label_h = metrics.label_height * zoom;
    let gap = metrics.section_gap * zoom;
    let ribbon_pad = metrics.ribbon_padding * zoom;
    let rows = metrics.visible_rows.max(1.0);
    let fit = (height - top - bottom - rows * label_h - (rows - 1.0) * gap) / rows;
    let scale = (fit * zoom).max(1.0) / height;
    let (strip_w, strip_h) = (width * scale, height * scale);
    let strip_x = (width - strip_w) / 2.0;

    // First pass: stack sections top-down from y = 0.
    let section_h = label_h + strip_h + gap;
    let active_row = strips.iter().position(|s| s.active).unwrap_or(0);
    let content_h = section_h * strips.len() as f64 - gap;

    // Keep the active strip centred when the stack overflows the display.
    let usable = height - top - bottom;
    let scroll = if content_h <= usable {
        -(usable - content_h) / 2.0
    } else {
        let centre = section_h * active_row as f64 + label_h + strip_h / 2.0;
        (centre - usable / 2.0).clamp(0.0, content_h - usable)
    };

    let placements = strips
        .iter()
        .enumerate()
        .map(|(row, strip)| {
            let section_y = top + section_h * row as f64 - scroll;
            let label = Rect::new(strip_x, section_y, strip_w, label_h);
            let viewport_overview = Rect::new(strip_x, section_y + label_h, strip_w, strip_h);
            let ribbon = Rect::new(
                ribbon_pad,
                viewport_overview.y,
                width - 2.0 * ribbon_pad,
                strip_h,
            );
            let row_offset = (row as f64 - active_row as f64) * height;
            let viewport_rest = Rect::new(0.0, row_offset, width, height);

            let windows = strip
                .windows
                .iter()
                .map(|window| {
                    let local = window.layout_frame;
                    let overview = Rect::new(
                        viewport_overview.x + local.x * scale,
                        viewport_overview.y + local.y * scale,
                        local.w * scale,
                        local.h * scale,
                    );
                    let rest = window
                        .real_frame
                        .filter(|_| strip.active)
                        .unwrap_or_else(|| local.offset(0.0, row_offset));
                    WindowPlacement {
                        key: window.key.clone(),
                        window_id: window.window_id,
                        rest,
                        overview,
                    }
                })
                .collect();

            StripPlacement {
                virtual_index: strip.virtual_index,
                active: strip.active,
                label,
                viewport_rest,
                viewport_overview,
                ribbon,
                windows,
            }
        })
        .collect();

    DisplayLayout {
        size,
        scale,
        strips: placements,
    }
}

/// What a click in the open overview landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit<K> {
    Window { key: K, virtual_index: u32 },
    Strip { virtual_index: u32 },
    Background,
}

/// Hit-tests a display-local point against the fully open layout. Windows
/// win over their strip, and are clipped to the strip's ribbon.
pub fn hit_test<K: Clone>(layout: &DisplayLayout<K>, x: f64, y: f64) -> Hit<K> {
    for strip in &layout.strips {
        if !strip.ribbon.contains(x, y) {
            continue;
        }
        // Topmost first: later windows are drawn above earlier ones.
        if let Some(window) = strip
            .windows
            .iter()
            .rev()
            .find(|w| w.overview.contains(x, y))
        {
            return Hit::Window {
                key: window.key.clone(),
                virtual_index: strip.virtual_index,
            };
        }
        return Hit::Strip {
            virtual_index: strip.virtual_index,
        };
    }
    Hit::Background
}

/// Re-anchors a layout on another row so closing into it lands its windows
/// on the full display while the other rows slide off. The target row's
/// windows rest at their layout frames (their real position after the
/// switch) rather than wherever they are parked now.
#[allow(clippy::cast_precision_loss, reason = "row counts are tiny")]
pub fn reanchor<K: Clone>(layout: &mut DisplayLayout<K>, virtual_index: u32) {
    let Some(anchor) = layout
        .strips
        .iter()
        .position(|s| s.virtual_index == virtual_index)
    else {
        return;
    };
    let height = layout.size.1;
    let scale = layout.scale;
    for (row, strip) in layout.strips.iter_mut().enumerate() {
        let row_offset = (row as f64 - anchor as f64) * height;
        let was_active = strip.active;
        strip.active = row == anchor;
        strip.viewport_rest = Rect::new(0.0, row_offset, layout.size.0, height);
        for window in &mut strip.windows {
            // Recover the layout frame from the overview frame.
            let local = Rect::new(
                (window.overview.x - strip.viewport_overview.x) / scale,
                (window.overview.y - strip.viewport_overview.y) / scale,
                window.overview.w / scale,
                window.overview.h / scale,
            );
            if !(was_active && strip.active) {
                window.rest = local.offset(0.0, row_offset);
            }
        }
    }
}

// ── Transition spring ───────────────────────────────────────────────────

/// Critically damped spring on the overview progress (0 = closed, 1 = open).
///
/// The same parameters are handed to `CASpringAnimation` (mass 1, stiffness
/// ω², damping 2ω) so Core Animation animates on the render server, while
/// this closed form tells the ECS where the animation is at any instant —
/// needed to retarget mid-flight and to resume a gesture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub from: f64,
    pub to: f64,
    /// Initial velocity in progress units per second.
    pub velocity: f64,
    pub omega: f64,
}

/// Seconds for the spring to get to within ~0.1% of its target.
pub const SPRING_RESPONSE: f64 = 0.3;

impl Spring {
    pub fn new(from: f64, to: f64, velocity: f64, response: f64) -> Self {
        Self {
            from,
            to,
            velocity,
            omega: std::f64::consts::TAU / response.max(0.01),
        }
    }

    pub fn value(&self, t: f64) -> f64 {
        let d = self.from - self.to;
        let w = self.omega;
        self.to + (-w * t).exp() * (d + (w * d + self.velocity) * t)
    }

    pub fn velocity_at(&self, t: f64) -> f64 {
        let d = self.from - self.to;
        let w = self.omega;
        (-w * t).exp() * (self.velocity - w * (w * d + self.velocity) * t)
    }

    pub fn stiffness(&self) -> f64 {
        self.omega * self.omega
    }

    pub fn damping(&self) -> f64 {
        2.0 * self.omega
    }

    /// `CASpringAnimation.initialVelocity` is in units of the total
    /// distance travelled per second.
    pub fn normalized_velocity(&self) -> f64 {
        let span = self.to - self.from;
        if span.abs() < 1e-6 {
            0.0
        } else {
            self.velocity / span
        }
    }

    /// Time until both displacement and velocity are negligible.
    #[cfg(test)]
    pub fn settling_duration(&self) -> f64 {
        let mut t = 0.0;
        while t < 3.0 {
            if (self.value(t) - self.to).abs() < 1e-3 && self.velocity_at(t).abs() < 1e-2 {
                return t;
            }
            t += 1.0 / 240.0;
        }
        t
    }
}

// ── Gesture scrubbing ───────────────────────────────────────────────────

/// Soft limit past either end while scrubbing, so dragging beyond fully
/// open/closed resists instead of stopping dead.
pub fn rubber_band(progress: f64) -> f64 {
    const LIMIT: f64 = 0.15;
    let band = |excess: f64| LIMIT * excess / (LIMIT + excess);
    if progress > 1.0 {
        1.0 + band(progress - 1.0)
    } else if progress < 0.0 {
        -band(-progress)
    } else {
        progress
    }
}

/// Whether a released gesture should settle open (`true`) or closed.
/// A decisive flick wins over position; otherwise project a short way
/// along the current velocity and round.
pub fn release_opens(progress: f64, velocity: f64) -> bool {
    const FLICK: f64 = 0.5;
    if velocity >= FLICK {
        true
    } else if velocity <= -FLICK {
        false
    } else {
        progress + 0.15 * velocity >= 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (f64, f64) = (1600.0, 1000.0);

    fn window(key: u32, x: f64, real: Option<Rect>) -> WindowInput<u32> {
        WindowInput {
            key,
            window_id: key.cast_signed(),
            layout_frame: Rect::new(x, 30.0, 800.0, 970.0),
            real_frame: real,
        }
    }

    fn strips() -> Vec<StripInput<u32>> {
        vec![
            StripInput {
                virtual_index: 0,
                active: false,
                windows: vec![window(1, 0.0, None)],
            },
            StripInput {
                virtual_index: 1,
                active: true,
                windows: vec![
                    window(2, 0.0, Some(Rect::new(0.0, 30.0, 800.0, 970.0))),
                    // Scrolled off to the right: real frame is a sliver.
                    window(3, 1700.0, Some(Rect::new(1590.0, 30.0, 800.0, 970.0))),
                ],
            },
        ]
    }

    #[test]
    fn strips_stack_vertically_at_the_same_scale() {
        let layout = layout_display(SIZE, &strips(), 1.0, &Metrics::default());
        let [a, b] = &layout.strips[..] else { panic!() };
        assert!(layout.scale > 0.3 && layout.scale < 0.5, "{}", layout.scale);
        assert!(a.viewport_overview.y < b.viewport_overview.y);
        assert_eq!(a.viewport_overview.w, b.viewport_overview.w);
        assert!((a.viewport_overview.w - SIZE.0 * layout.scale).abs() < 1e-9);
        // Everything fits on screen.
        assert!(b.viewport_overview.y + b.viewport_overview.h <= SIZE.1);
        assert!(a.label.y >= 0.0);
    }

    #[test]
    fn active_strip_rests_on_real_frames_others_slide_from_offscreen() {
        let layout = layout_display(SIZE, &strips(), 1.0, &Metrics::default());
        let active = &layout.strips[1];
        assert_eq!(
            active.windows[1].rest,
            Rect::new(1590.0, 30.0, 800.0, 970.0)
        );
        assert_eq!(active.viewport_rest, Rect::new(0.0, 0.0, SIZE.0, SIZE.1));
        // The row above rests one display height up.
        let above = &layout.strips[0];
        assert_eq!(
            above.windows[0].rest,
            Rect::new(0.0, 30.0 - SIZE.1, 800.0, 970.0)
        );
    }

    #[test]
    fn overview_frames_keep_off_viewport_columns_on_the_ribbon() {
        let layout = layout_display(SIZE, &strips(), 1.0, &Metrics::default());
        let strip = &layout.strips[1];
        let off = &strip.windows[1].overview;
        // Starts right of the scaled viewport, i.e. the ribbon shows it.
        assert!(off.x > strip.viewport_overview.x + strip.viewport_overview.w);
        assert!(off.x < strip.ribbon.x + strip.ribbon.w);
    }

    #[test]
    fn active_row_is_centred_when_rows_overflow() {
        let many: Vec<_> = (0..6)
            .map(|i| StripInput::<u32> {
                virtual_index: i,
                active: i == 5,
                windows: vec![],
            })
            .collect();
        let layout = layout_display(SIZE, &many, 1.0, &Metrics::default());
        let last = layout.strips.last().unwrap();
        assert!(last.viewport_overview.y >= 0.0);
        assert!(last.viewport_overview.y + last.viewport_overview.h <= SIZE.1);
        assert!(layout.strips[0].viewport_overview.y < 0.0);
    }

    #[test]
    fn hit_test_prefers_windows_and_respects_ribbon() {
        let layout = layout_display(SIZE, &strips(), 1.0, &Metrics::default());
        let w = &layout.strips[1].windows[0].overview;
        assert_eq!(
            hit_test(&layout, w.x + 5.0, w.y + 5.0),
            Hit::Window {
                key: 2,
                virtual_index: 1
            }
        );
        let v = &layout.strips[0].viewport_overview;
        assert_eq!(
            hit_test(&layout, v.x + v.w - 2.0, v.y + v.h - 2.0),
            Hit::Strip { virtual_index: 0 }
        );
        assert_eq!(hit_test(&layout, 2.0, 2.0), Hit::Background);
    }

    #[test]
    fn reanchor_moves_target_row_to_full_screen() {
        let mut layout = layout_display(SIZE, &strips(), 1.0, &Metrics::default());
        reanchor(&mut layout, 0);
        let target = &layout.strips[0];
        assert!(target.active);
        assert_eq!(target.viewport_rest, Rect::new(0.0, 0.0, SIZE.0, SIZE.1));
        let r = target.windows[0].rest;
        assert!((r.x - 0.0).abs() < 1e-6 && (r.y - 30.0).abs() < 1e-6);
        // The previously active row now rests below.
        assert!((layout.strips[1].windows[0].rest.y - (30.0 + SIZE.1)).abs() < 1e-6);
    }

    #[test]
    fn spring_starts_at_from_and_settles_at_to() {
        let spring = Spring::new(0.0, 1.0, 0.0, SPRING_RESPONSE);
        assert!((spring.value(0.0) - 0.0).abs() < 1e-12);
        assert!(spring.velocity_at(0.0).abs() < 1e-12);
        let settle = spring.settling_duration();
        assert!(settle > 0.1 && settle < 1.0, "{settle}");
        assert!((spring.value(settle) - 1.0).abs() < 1e-3);
        // Critically damped: never overshoots from rest.
        assert!((0..200).all(|i| spring.value(f64::from(i) / 200.0) <= 1.0 + 1e-9));
    }

    #[test]
    fn spring_carries_initial_velocity() {
        let spring = Spring::new(0.4, 1.0, 3.0, SPRING_RESPONSE);
        assert!((spring.velocity_at(0.0) - 3.0).abs() < 1e-9);
        assert!((spring.normalized_velocity() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn rubber_band_is_identity_inside_and_bounded_outside() {
        assert_eq!(rubber_band(0.3), 0.3);
        assert!(rubber_band(5.0) < 1.15);
        assert!(rubber_band(-5.0) > -0.15);
        assert!(rubber_band(1.1) > 1.0);
    }

    #[test]
    fn release_rule() {
        assert!(release_opens(0.2, 1.0));
        assert!(!release_opens(0.9, -1.0));
        assert!(release_opens(0.6, 0.0));
        assert!(!release_opens(0.4, 0.0));
    }
}
