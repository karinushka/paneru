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

mod capture;
mod layers;
mod live;
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

    /// Whether the two rectangles overlap (touching edges don't count).
    pub fn intersects(&self, other: &Self) -> bool {
        self.intersection_area(other) > 0.0
    }

    /// Area of overlap between `self` and `other` (`0.0` when disjoint).
    pub fn intersection_area(&self, other: &Self) -> f64 {
        let w = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let h = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub const fn centre(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn offset(&self, dx: f64, dy: f64) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// Shrinks about the centre so the width loses `gap`, keeping the aspect.
    pub fn shrink(&self, gap: f64) -> Self {
        if self.w <= gap * 2.0 {
            return *self;
        }
        let k = (self.w - gap) / self.w;
        let (w, h) = (self.w * k, self.h * k);
        Self::new(
            self.x + (self.w - w) / 2.0,
            self.y + (self.h - h) / 2.0,
            w,
            h,
        )
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

/// Whether an overview panel is up. Read by the event tap (off the ECS) so
/// scroll gestures go to the overview instead of paneru's strip scrolling,
/// its post-swipe suppression, or the window underneath.
pub static OVERVIEW_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the event tap should intercept bare `Escape` and `Return` for the
/// overview. `true` while opening, open, or actively scrubbed past zero — even
/// after a focus keybind hands `AppKit` key-window status to another app — and
/// `false` during touch-down prebuilds and the closing animation.
pub static OVERVIEW_ACCEPTS_KEYS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) static OVERVIEW_OPEN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Inputs ──────────────────────────────────────────────────────────────

/// One tiled window as the ECS sees it.
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
    pub focused: bool,
}

/// One floating (`Unmanaged::Floating`) window on this display.
#[derive(Clone, Debug)]
pub struct FloatingInput<K> {
    pub key: K,
    pub window_id: i32,
    /// Where the floating window sits on screen right now, in display-local
    /// coordinates.
    pub frame: Rect,
    pub focused: bool,
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
    /// Space between neighbouring windows while open, in overview points.
    /// Cards shrink about their centre (keeping their aspect) to make it.
    pub card_gap: f64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            top_padding: 48.0,
            bottom_padding: 40.0,
            label_height: 26.0,
            section_gap: 20.0,
            ribbon_padding: 24.0,
            visible_rows: 2.0,
            card_gap: 10.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WindowPlacement<K> {
    pub key: K,
    pub window_id: i32,
    /// Display-local frame the window has when its row is active.
    pub layout: Rect,
    /// Frame while closed (progress 0).
    pub rest: Rect,
    /// Frame while open (progress 1), before the strip's pan is applied.
    pub overview: Rect,
    pub focused: bool,
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
    /// The strip's horizontal band: the full display width minus padding, so
    /// off-viewport columns stay visible on either side.
    pub ribbon: Rect,
    /// User pan of the tiled columns, in overview points (positive = right).
    pub pan: f64,
    pub windows: Vec<WindowPlacement<K>>,
}

#[derive(Clone, Debug)]
pub struct DisplayLayout<K> {
    pub size: (f64, f64),
    /// Display → strip scale factor.
    #[allow(dead_code, reason = "useful to callers; kept for the zoom work")]
    pub scale: f64,
    /// User scroll of the whole stack, in points (positive = content up).
    pub scroll: f64,
    /// Vertical margins the stack scrolls within.
    pub margins: (f64, f64),
    /// Horizontal span (display-local) paneru keeps a focused window inside;
    /// see `reanchor`. Defaults to the full display width.
    pub work_area: (f64, f64),
    pub strips: Vec<StripPlacement<K>>,
    /// Floating windows parked in a pinned top-right shelf above the strips.
    pub floating: Vec<WindowPlacement<K>>,
}

/// Height of the top-right floating shelf as a fraction of the display height.
const FLOATING_SHELF_HEIGHT_RATIO: f64 = 0.12;

/// Top inset of the floating shelf (clears the macOS menu bar / notch height).
const FLOATING_SHELF_TOP: f64 = 36.0;

/// Aspect ratio clamp `(min, max)` so extreme sliver/banner floating windows
/// stay clickable and legible in the shelf.
const FLOATING_ASPECT_RANGE: (f64, f64) = (0.4, 3.0);

/// Lays out floating windows into a right-aligned top-right shelf, returning
/// `(reserved_top_margin, placements)`. When `floating` is empty, `top` stays
/// at `metrics.top_padding * zoom`.
#[allow(clippy::cast_precision_loss, reason = "window counts are small")]
fn layout_floating_shelf<K: Clone>(
    size: (f64, f64),
    floating: &[FloatingInput<K>],
    zoom: f64,
    metrics: &Metrics,
) -> (f64, Vec<WindowPlacement<K>>) {
    let default_top = metrics.top_padding * zoom;
    if floating.is_empty() {
        return (default_top, Vec::new());
    }
    let (width, height) = size;
    let ribbon_pad = metrics.ribbon_padding * zoom;
    let shelf_top = FLOATING_SHELF_TOP * zoom;
    let shelf_h = (height * FLOATING_SHELF_HEIGHT_RATIO * zoom).max(48.0 * zoom);
    let gap = metrics.card_gap * zoom;

    let natural_widths: Vec<f64> = floating
        .iter()
        .map(|f| {
            let aspect = (f.frame.w / f.frame.h.max(1.0))
                .clamp(FLOATING_ASPECT_RANGE.0, FLOATING_ASPECT_RANGE.1);
            shelf_h * aspect
        })
        .collect();
    let natural_sum: f64 = natural_widths.iter().sum();
    let gap_total = gap * (floating.len().saturating_sub(1) as f64);
    let max_w = (width - 2.0 * ribbon_pad).max(1.0);
    let fit_k = (max_w / (natural_sum + gap_total).max(1.0)).min(1.0);

    let thumb_h = shelf_h * fit_k;
    let y = shelf_top + (shelf_h - thumb_h) / 2.0;
    let total_w = (natural_sum + gap_total) * fit_k;
    let start_x = width - ribbon_pad - total_w;
    let placements = floating
        .iter()
        .zip(natural_widths)
        .scan(start_x, |x, (f, natural_w)| {
            let w = natural_w * fit_k;
            let overview = Rect::new(*x, y, w, thumb_h);
            *x += w + gap * fit_k;
            Some(WindowPlacement {
                key: f.key.clone(),
                window_id: f.window_id,
                layout: f.frame,
                rest: f.frame,
                overview,
                focused: f.focused,
            })
        })
        .collect();
    let reserved_top = (shelf_top + shelf_h + metrics.section_gap * zoom).max(default_top);
    (reserved_top, placements)
}

/// Lays out one display's rows as niri does: a vertical stack of strips,
/// each a scaled copy of the full display, with the active strip kept in
/// view. Rows other than the active one rest one display height above or
/// below it, so on open they slide in from off-screen while shrinking.
#[allow(clippy::cast_precision_loss, reason = "row counts are tiny")]
pub fn layout_display<K: Clone>(
    size: (f64, f64),
    strips: &[StripInput<K>],
    floating: &[FloatingInput<K>],
    zoom: f64,
    metrics: &Metrics,
) -> DisplayLayout<K> {
    let (width, height) = size;
    let zoom = zoom.clamp(0.5, 1.5);
    let (top, floating_placements) = layout_floating_shelf(size, floating, zoom, metrics);
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

    // Every display opens with its active row in the middle, as niri does,
    // even when that row is the first or last one (`scroll_range` lets the
    // user scroll the rest into view).
    let usable = height - top - bottom;
    let centre = section_h * active_row as f64 + label_h + strip_h / 2.0;
    let scroll = centre - usable / 2.0;

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
                    )
                    .shrink(metrics.card_gap * zoom);
                    let rest = window
                        .real_frame
                        .filter(|_| strip.active)
                        .unwrap_or_else(|| local.offset(0.0, row_offset));
                    WindowPlacement {
                        key: window.key.clone(),
                        window_id: window.window_id,
                        layout: local,
                        rest,
                        overview,
                        focused: window.focused,
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
                pan: 0.0,
                windows,
            }
        })
        .collect();

    DisplayLayout {
        size,
        scale,
        scroll: 0.0,
        margins: (top, bottom),
        work_area: (0.0, width),
        strips: placements,
        floating: floating_placements,
    }
}

impl<K> DisplayLayout<K> {
    /// Row index whose band contains the display-local point.
    pub fn strip_at(&self, x: f64, y: f64) -> Option<usize> {
        let y = y + self.scroll;
        self.strips.iter().position(|strip| {
            let band = &strip.ribbon;
            x >= 0.0 && x < self.size.0 && y >= strip.label.y && y < band.y + band.h
        })
    }

    /// Scrolls the stack. The range always lets the first and the last row
    /// be brought to the middle of the display, and extends to the margins
    /// when the stack overflows. Returns whether anything moved.
    pub fn scroll_by(&mut self, dy: f64) -> bool {
        let (min, max) = self.scroll_range();
        let next = (self.scroll + dy).clamp(min, max);
        let moved = (next - self.scroll).abs() > f64::EPSILON;
        self.scroll = next;
        moved
    }

    /// The `(min, max)` scroll the stack can take.
    pub fn scroll_range(&self) -> (f64, f64) {
        let (Some(first), Some(last)) = (self.strips.first(), self.strips.last()) else {
            return (0.0, 0.0);
        };
        let (top, bottom) = self.margins;
        let middle = top + (self.size.1 - top - bottom) / 2.0;
        let centre = |s: &StripPlacement<K>| s.viewport_overview.centre().1;
        let content_top = first.label.y;
        let content_bottom = last.viewport_overview.y + last.viewport_overview.h;
        let min = (content_top - top).min(centre(first) - middle).min(0.0);
        let max = (content_bottom - (self.size.1 - bottom))
            .max(centre(last) - middle)
            .max(0.0);
        (min, max)
    }

    /// Pans one strip's tiled columns within the range paneru itself lets a
    /// strip scroll (its edge rule): a strip wider than the work area keeps
    /// its first column at or past the left edge and its last at or past the
    /// right edge; a narrower one stays pinned left. The current offset is
    /// always allowed, whatever placed it.
    pub fn pan_by(&mut self, row: usize, dx: f64) -> bool {
        let (lo, hi) = self.pan_range(row);
        let Some(strip) = self.strips.get_mut(row) else {
            return false;
        };
        let next = (strip.pan + dx).clamp(lo, hi);
        let moved = (next - strip.pan).abs() > f64::EPSILON;
        strip.pan = next;
        moved
    }

    /// The `(min, max)` pan of one row, in overview points.
    pub fn pan_range(&self, row: usize) -> (f64, f64) {
        let Some(strip) = self.strips.get(row) else {
            return (0.0, 0.0);
        };
        let (left, right) = strip
            .windows
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, r), w| {
                (l.min(w.layout.x), r.max(w.layout.x + w.layout.w))
            });
        if left > right {
            return (0.0, 0.0);
        }
        // In display points, then scaled: pans are overview points.
        let (area_left, area_right) = self.work_area;
        let pinned_left = area_left - left;
        let lo = if right - left > area_right - area_left {
            area_right - right
        } else {
            pinned_left
        };
        (lo.min(0.0) * self.scale, pinned_left.max(0.0) * self.scale)
    }

    /// A row's pan in display points: how far committing it moves the strip.
    pub fn pan_points(&self, row: usize) -> f64 {
        self.strips
            .get(row)
            .map_or(0.0, |strip| strip.pan / self.scale.max(f64::EPSILON))
    }
}

/// The row whose strip sits nearest the middle of the screen at the
/// current scroll: the one the overview closes into.
pub fn middle_row<K>(layout: &DisplayLayout<K>) -> Option<usize> {
    let (top, bottom) = layout.margins;
    let middle = top + (layout.size.1 - top - bottom) / 2.0 + layout.scroll;
    let distance = |s: &StripPlacement<K>| (s.viewport_overview.centre().1 - middle).abs();
    layout
        .strips
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
        .map(|(row, _)| row)
}

/// Whether two layouts put everything in the same place (within half a
/// point), so a relayout can be skipped.
pub fn same_geometry<K: PartialEq>(a: &DisplayLayout<K>, b: &DisplayLayout<K>) -> bool {
    let close = |x: &Rect, y: &Rect| {
        (x.x - y.x).abs() < 0.5
            && (x.y - y.y).abs() < 0.5
            && (x.w - y.w).abs() < 0.5
            && (x.h - y.h).abs() < 0.5
    };
    let same_window = |w: &WindowPlacement<K>, v: &WindowPlacement<K>| {
        w.key == v.key && w.focused == v.focused && close(&w.overview, &v.overview)
    };
    a.strips.len() == b.strips.len()
        && a.floating.len() == b.floating.len()
        && a.floating
            .iter()
            .zip(&b.floating)
            .all(|(w, v)| same_window(w, v))
        && a.strips.iter().zip(&b.strips).all(|(s, t)| {
            s.virtual_index == t.virtual_index
                && s.active == t.active
                && close(&s.viewport_overview, &t.viewport_overview)
                && s.windows.len() == t.windows.len()
                && s.windows
                    .iter()
                    .zip(&t.windows)
                    .all(|(w, v)| same_window(w, v))
        })
}

/// What a click in the open overview landed on.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit<K> {
    Window { key: K, virtual_index: u32 },
    Floating { key: K },
    Strip { virtual_index: u32 },
    Background,
}

/// Hit-tests a display-local point against the fully open layout. Pinned
/// floating shelf cards win first; tiled windows win over their strip and are
/// clipped to the strip's ribbon.
pub fn hit_test<K: Clone>(layout: &DisplayLayout<K>, x: f64, y: f64) -> Hit<K> {
    if let Some(window) = layout
        .floating
        .iter()
        .rev()
        .find(|w| w.overview.contains(x, y))
    {
        return Hit::Floating {
            key: window.key.clone(),
        };
    }
    let y = y + layout.scroll;
    layout
        .strips
        .iter()
        .find(|strip| strip.ribbon.contains(x, y))
        .map_or(Hit::Background, |strip| {
            strip
                .windows
                .iter()
                .rev()
                .find(|w| w.overview.contains(x - strip.pan, y))
                .map_or(
                    Hit::Strip {
                        virtual_index: strip.virtual_index,
                    },
                    |window| Hit::Window {
                        key: window.key.clone(),
                        virtual_index: strip.virtual_index,
                    },
                )
        })
}

/// Windows whose card is on screen (or within `margin` of it, so cards just
/// scrolled away don't stop and restart) once the overview is fully open,
/// given the current scroll and pans.
pub fn visible_windows<K>(layout: &DisplayLayout<K>, margin: f64) -> Vec<i32> {
    let screen = Rect::new(
        -margin,
        layout.scroll - margin,
        layout.size.0 + 2.0 * margin,
        layout.size.1 + 2.0 * margin,
    );
    layout
        .strips
        .iter()
        .flat_map(|strip| {
            strip
                .windows
                .iter()
                .filter(move |w| w.overview.offset(strip.pan, 0.0).intersects(&screen))
        })
        .chain(&layout.floating)
        .map(|w| w.window_id)
        .collect()
}

/// Re-anchors a layout on another row so closing into it lands its windows
/// where paneru will really put them while the other rows slide off.
///
/// Tiled windows rest at their layout frames. When `focus` names a window
/// of that row, the row is first shifted the way paneru scrolls a strip to
/// expose a newly focused window (by the shortfall only, see
/// `layout::origin_exposing`), so picking a card that was scrolled out of
/// view doesn't land on the old scroll and then jump. Floating windows live
/// outside the strips (`layout.floating`) and keep their real rest frames.
#[allow(clippy::cast_precision_loss, reason = "row counts are tiny")]
pub fn reanchor<K: Clone + PartialEq>(
    layout: &mut DisplayLayout<K>,
    virtual_index: u32,
    focus: Option<&K>,
) {
    let Some(anchor) = layout
        .strips
        .iter()
        .position(|s| s.virtual_index == virtual_index)
    else {
        return;
    };
    let height = layout.size.1;
    let work_area = layout.work_area;
    let scale = layout.scale.max(f64::EPSILON);
    for (row, strip) in layout.strips.iter_mut().enumerate() {
        let row_offset = (row as f64 - anchor as f64) * height;
        let was_active = strip.active;
        strip.active = row == anchor;
        strip.viewport_rest = Rect::new(0.0, row_offset, layout.size.0, height);
        // What the user panned to is where the strip is left.
        let pan = strip.pan / scale;
        let shift = if strip.active {
            focus
                .and_then(|key| strip.windows.iter().find(|w| &w.key == key))
                .map_or(0.0, |w| {
                    exposing_shift(&w.layout.offset(pan, 0.0), work_area)
                })
        } else {
            0.0
        };
        // Staying on the row that was open with nothing to scroll: the
        // windows go back exactly where they were taken from.
        if !(was_active && strip.active && (shift + pan).abs() < 0.5) {
            let dx = pan + shift;
            for window in &mut strip.windows {
                window.rest = window.layout.offset(dx, row_offset);
            }
        }
    }
}

/// Horizontal shift that brings `frame` inside `area`, moving only by what
/// hangs over an edge — the same clamp as `layout::clamp_origin_to_viewport`
/// (a window wider than the area may sit anywhere that keeps it covering).
pub fn exposing_shift(frame: &Rect, area: (f64, f64)) -> f64 {
    let (left, right) = area;
    let far = right - frame.w;
    let (min, max) = (left.min(far), left.max(far));
    frame.x.clamp(min, max) - frame.x
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

/// Share of a transition's travel left when it ends; see
/// [`Spring::visible_duration`].
pub const VISIBLE_REMAINDER: f64 = 0.005;

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

    /// How long the transition is shown for: until what's left of the travel
    /// stays too small to see ([`VISIBLE_REMAINDER`]), well before the spring
    /// settles in full. Scanning from the end avoids truncating a high-velocity
    /// transition at an early overshoot crossing of `to`.
    pub fn visible_duration(&self) -> f64 {
        const STEP: f64 = 1.0 / 240.0;
        const MAX_STEPS: u32 = 720;
        let left = VISIBLE_REMAINDER * (self.to - self.from).abs().max(f64::EPSILON);
        let settled_step = (0..MAX_STEPS)
            .rev()
            .find(|&step| (self.value(f64::from(step) * STEP) - self.to).abs() >= left)
            .map_or(0, |step| step + 1);
        // A zero duration means Core Animation's default (0.25 s).
        (f64::from(settled_step) * STEP).max(1.0 / 120.0)
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
            focused: false,
        }
    }

    fn layout_strips(rows: &[StripInput<u32>]) -> DisplayLayout<u32> {
        layout_display(SIZE, rows, &[], 1.0, &Metrics::default())
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
    fn rect_overlap() {
        let a = Rect::new(0.0, 0.0, 50.0, 50.0);
        assert!(a.intersects(&Rect::new(40.0, 40.0, 20.0, 20.0)));
        // Side by side columns only touch.
        assert!(!a.intersects(&Rect::new(50.0, 0.0, 50.0, 50.0)));
    }

    #[test]
    fn strips_stack_vertically_at_the_same_scale() {
        let layout = layout_strips(&strips());
        let [a, b] = &layout.strips[..] else { panic!() };
        assert!(layout.scale > 0.3 && layout.scale < 0.5, "{}", layout.scale);
        assert!(a.viewport_overview.y < b.viewport_overview.y);
        assert_eq!(a.viewport_overview.w, b.viewport_overview.w);
        assert!((a.viewport_overview.w - SIZE.0 * layout.scale).abs() < 1e-9);
        // The active row (the second) opens fully on screen.
        assert!(b.label.y >= 0.0);
        assert!(b.viewport_overview.y + b.viewport_overview.h <= SIZE.1);
    }

    #[test]
    fn active_strip_rests_on_real_frames_others_slide_from_offscreen() {
        let layout = layout_strips(&strips());
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
        let layout = layout_strips(&strips());
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
        let layout = layout_strips(&many);
        let (top, bottom) = layout.margins;
        let middle = top + (SIZE.1 - top - bottom) / 2.0;
        assert!((centre_y(&layout, 5) - middle).abs() < 1e-6);
        assert!(layout.strips[0].viewport_overview.y < 0.0);
    }

    #[test]
    fn the_row_in_the_middle_is_the_one_closed_into() {
        let rows: Vec<_> = (0..3)
            .map(|i| StripInput::<u32> {
                virtual_index: i,
                active: i == 1,
                windows: vec![],
            })
            .collect();
        let mut layout = layout_strips(&rows);
        assert_eq!(middle_row(&layout), Some(1));
        let step = layout.strips[2].viewport_overview.y - layout.strips[1].viewport_overview.y;
        layout.scroll_by(step);
        assert_eq!(middle_row(&layout), Some(2));
        layout.scroll_by(-2.0 * step);
        assert_eq!(middle_row(&layout), Some(0));
        // Short of half a row, it stays.
        layout.scroll_by(0.4 * step);
        assert_eq!(middle_row(&layout), Some(0));
    }

    #[test]
    fn active_row_opens_in_the_middle_even_first_or_last() {
        let metrics = Metrics::default();
        for count in [1, 2, 6] {
            for active in [0, count - 1] {
                let rows: Vec<_> = (0..count)
                    .map(|i| StripInput::<u32> {
                        virtual_index: i,
                        active: i == active,
                        windows: vec![],
                    })
                    .collect();
                let layout = layout_display(SIZE, &rows, &[], 1.0, &metrics);
                let (top, bottom) = layout.margins;
                let middle = top + (SIZE.1 - top - bottom) / 2.0;
                assert!(
                    (centre_y(&layout, active as usize) - middle).abs() < 1e-6,
                    "{count} rows, active {active}"
                );
            }
        }
    }

    #[test]
    fn hit_test_prefers_windows_and_respects_ribbon() {
        let layout = layout_strips(&strips());
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
        let mut layout = layout_strips(&strips());
        reanchor(&mut layout, 0, None);
        let target = &layout.strips[0];
        assert!(target.active);
        assert_eq!(target.viewport_rest, Rect::new(0.0, 0.0, SIZE.0, SIZE.1));
        let r = target.windows[0].rest;
        assert!((r.x - 0.0).abs() < 1e-6 && (r.y - 30.0).abs() < 1e-6);
        // The previously active row now rests below.
        assert!((layout.strips[1].windows[0].rest.y - (30.0 + SIZE.1)).abs() < 1e-6);
    }

    #[test]
    fn cards_get_gaps_without_changing_aspect() {
        let layout = layout_strips(&strips());
        let strip = &layout.strips[1];
        let (a, b) = (&strip.windows[0], &strip.windows[1]);
        // Columns were edge to edge (0..800, 800..) in layout space.
        let gap_a_end = a.overview.x + a.overview.w;
        let neighbour = layout_display(
            SIZE,
            &[StripInput {
                virtual_index: 0,
                active: true,
                windows: vec![window(9, 800.0, None)],
            }],
            &[],
            1.0,
            &Metrics::default(),
        );
        let b_start = neighbour.strips[0].windows[0].overview.x;
        assert!(b_start - gap_a_end > 9.0, "{}", b_start - gap_a_end);
        let unshrunk = 800.0 / 970.0;
        assert!((a.overview.w / a.overview.h - unshrunk).abs() < 1e-9);
        assert!(b.overview.w > 0.0);
    }

    #[test]
    fn pan_and_scroll_clamp_and_shift_hit_testing() {
        let mut rows = strips();
        rows[1].windows[1].layout_frame.x = 3000.0;
        let mut layout = layout_strips(&rows);
        // Row 1 has a column far off to the right: panning left is allowed
        // until that column's right edge meets the screen's (paneru's edge
        // rule), panning right isn't (nothing beyond the left edge).
        assert!(layout.pan_by(1, -10_000.0));
        let strip = &layout.strips[1];
        let off = strip.windows[1].overview;
        let pan = strip.pan;
        let far = strip.windows[1].layout;
        assert!((pan - (SIZE.0 - (far.x + far.w)) * layout.scale).abs() < 1e-6);
        assert!((layout.pan_points(1) - (SIZE.0 - (far.x + far.w))).abs() < 1e-6);
        assert!(!layout.pan_by(1, -1.0));
        assert!(!layout.pan_by(1, 0.0));
        let (x, y) = (off.x + pan + 5.0, off.y + 5.0);
        assert_eq!(
            hit_test(&layout, x, y),
            Hit::Window {
                key: 3,
                virtual_index: 1
            }
        );
        // The last row opens in the middle, which is as far down as the
        // stack scrolls; it still scrolls up to the first row.
        assert!(!layout.scroll_by(500.0));
        assert!(layout.scroll_by(-500.0));
    }

    fn centre_y(layout: &DisplayLayout<u32>, row: usize) -> f64 {
        let v = &layout.strips[row].viewport_overview;
        v.y + v.h / 2.0 - layout.scroll
    }

    #[test]
    fn scroll_reaches_the_middle_for_every_row_when_rows_fit() {
        let mut layout = layout_strips(&strips());
        let (top, bottom) = layout.margins;
        let middle = top + (SIZE.1 - top - bottom) / 2.0;
        layout.scroll_by(-10_000.0);
        assert!((centre_y(&layout, 0) - middle).abs() < 1e-6);
        layout.scroll_by(10_000.0);
        assert!((centre_y(&layout, 1) - middle).abs() < 1e-6);
        // Scrolled to the last row, hit testing follows the content.
        let w = layout.strips[1].windows[0].overview;
        assert_eq!(
            hit_test(&layout, w.x + 5.0, w.y - layout.scroll + 5.0),
            Hit::Window {
                key: 2,
                virtual_index: 1
            }
        );
    }

    #[test]
    fn visible_windows_follow_scroll_and_pan() {
        let many: Vec<_> = (0..8)
            .map(|i| StripInput::<u32> {
                virtual_index: i,
                active: i == 0,
                windows: vec![window(i, 0.0, None), window(100 + i, 6000.0, None)],
            })
            .collect();
        let mut layout = layout_strips(&many);
        layout.scroll_by(-10_000.0);
        let top = visible_windows(&layout, 0.0);
        assert!(top.contains(&0));
        assert!(!top.contains(&7), "the last row is off screen: {top:?}");
        // Far-right columns only show once their row is panned to them.
        assert!(!top.contains(&100));
        assert!(layout.pan_by(0, -10_000.0));
        assert!(visible_windows(&layout, 0.0).contains(&100));
        layout.scroll_by(10_000.0);
        let bottom = visible_windows(&layout, 0.0);
        assert!(bottom.contains(&7));
        assert!(
            !bottom.contains(&0),
            "the first row is off screen: {bottom:?}"
        );
        // A margin keeps cards just past the edge.
        assert!(visible_windows(&layout, 10_000.0).contains(&0));
    }

    #[test]
    fn scroll_range_covers_overflowing_stacks_to_the_margins() {
        let many: Vec<_> = (0..6)
            .map(|i| StripInput::<u32> {
                virtual_index: i,
                active: i == 0,
                windows: vec![],
            })
            .collect();
        let mut layout = layout_strips(&many);
        layout.scroll_by(10_000.0);
        let last = &layout.strips[5].viewport_overview;
        let (_, bottom) = layout.margins;
        // The last row can at least reach the bottom margin.
        assert!(last.y + last.h - layout.scroll <= SIZE.1 - bottom + 1e-6);
    }

    #[test]
    fn exposing_shift_matches_paneru_clamp() {
        let area = (0.0, 1000.0);
        let frame = |x, w| Rect::new(x, 0.0, w, 700.0);
        // Fully off to the left comes back to the edge.
        assert_eq!(exposing_shift(&frame(-600.0, 400.0), area), 600.0);
        // Past the right edge moves only by the overhang.
        assert_eq!(exposing_shift(&frame(800.0, 400.0), area), -200.0);
        // Already visible: no move.
        assert_eq!(exposing_shift(&frame(100.0, 400.0), area), 0.0);
        // Oversized windows only move until they cover the area.
        assert_eq!(exposing_shift(&frame(300.0, 2048.0), (0.0, 1024.0)), -300.0);
        assert_eq!(
            exposing_shift(&frame(-1600.0, 2048.0), (0.0, 1024.0)),
            576.0
        );
        assert_eq!(exposing_shift(&frame(-600.0, 2048.0), (0.0, 1024.0)), 0.0);
    }

    #[test]
    fn picking_an_offscreen_card_in_the_open_row_lands_on_the_scrolled_strip() {
        let mut layout = layout_strips(&strips());
        // Window 3 sits at 1700..2500, past the right edge of a 1600 display.
        reanchor(&mut layout, 1, Some(&3));
        let row = &layout.strips[1];
        assert!(row.active);
        // Paneru scrolls the strip left by the overhang (900).
        assert!((row.windows[1].rest.x - 800.0).abs() < 1e-6);
        assert!((row.windows[0].rest.x - -900.0).abs() < 1e-6);
        assert!((row.windows[0].rest.y - 30.0).abs() < 1e-6);
    }

    #[test]
    fn picking_a_visible_card_in_the_open_row_keeps_the_real_frames() {
        let mut layout = layout_strips(&strips());
        reanchor(&mut layout, 1, Some(&2));
        let row = &layout.strips[1];
        assert_eq!(row.windows[0].rest, Rect::new(0.0, 30.0, 800.0, 970.0));
        // The parked sliver stays parked.
        assert_eq!(row.windows[1].rest, Rect::new(1590.0, 30.0, 800.0, 970.0));
    }

    #[test]
    fn picking_an_offscreen_card_in_a_hidden_row_activates_and_scrolls_it() {
        let mut rows = strips();
        rows[0].windows = vec![window(1, 0.0, None), window(4, -1200.0, None)];
        let mut layout = layout_strips(&rows);
        reanchor(&mut layout, 0, Some(&4));
        let (target, other) = (&layout.strips[0], &layout.strips[1]);
        assert!(target.active && !other.active);
        assert!((target.windows[1].rest.x - 0.0).abs() < 1e-6);
        assert!((target.windows[0].rest.x - 1200.0).abs() < 1e-6);
        // The row that was open slides off below at its layout frames.
        assert!((other.windows[1].rest.x - 1700.0).abs() < 1e-6);
        assert!((other.windows[1].rest.y - (30.0 + SIZE.1)).abs() < 1e-6);
    }

    #[test]
    fn exposing_respects_the_work_area() {
        let mut layout = layout_strips(&strips());
        layout.work_area = (8.0, 1592.0);
        reanchor(&mut layout, 1, Some(&3));
        assert!((layout.strips[1].windows[1].rest.x - 792.0).abs() < 1e-6);
    }

    #[test]
    fn floating_windows_park_in_top_right_shelf_without_overlapping_strips() {
        let float_a = Rect::new(400.0, 200.0, 500.0, 400.0);
        let float_b = Rect::new(900.0, 300.0, 300.0, 300.0);
        let floating = vec![
            FloatingInput {
                key: 7,
                window_id: 7,
                frame: float_a,
                focused: false,
            },
            FloatingInput {
                key: 8,
                window_id: 8,
                frame: float_b,
                focused: true,
            },
        ];
        let metrics = Metrics::default();
        let without = layout_display(SIZE, &strips(), &[], 1.0, &metrics);
        assert_eq!(without.margins.0, metrics.top_padding);
        assert!(without.floating.is_empty());

        let mut layout = layout_display(SIZE, &strips(), &floating, 1.0, &metrics);
        assert_eq!(layout.floating.len(), 2);

        // Rest frames stay on the real screen windows; overview frames park in
        // a top-right shelf (~12% display height) preserving aspect ratio.
        let (a, b) = (&layout.floating[0], &layout.floating[1]);
        assert_eq!(a.rest, float_a);
        assert_eq!(b.rest, float_b);
        assert!(!a.focused && b.focused);
        assert!((a.overview.h - SIZE.1 * 0.12).abs() < 1e-6);
        assert!((b.overview.h - SIZE.1 * 0.12).abs() < 1e-6);
        assert!((a.overview.w / a.overview.h - 1.25).abs() < 1e-6);
        assert!((b.overview.w / b.overview.h - 1.0).abs() < 1e-6);
        assert!(a.overview.x + a.overview.w < b.overview.x);
        assert!((b.overview.x + b.overview.w - (SIZE.0 - metrics.ribbon_padding)).abs() < 1e-6);

        // Reserved top band keeps the active row's label and strip strictly
        // below the floating shelf.
        let (a_ov, b_ov) = (a.overview, b.overview);
        let shelf_bottom = a_ov.y + a_ov.h;
        assert!(layout.margins.0 > shelf_bottom);
        assert!(layout.strips[1].label.y >= layout.margins.0);

        // Hit-testing hits the pinned shelf regardless of scroll or pan.
        layout.pan_by(1, -500.0);
        layout.scroll_by(-200.0);
        assert_eq!(
            hit_test(&layout, a_ov.x + 5.0, a_ov.y + 5.0),
            Hit::Floating { key: 7 }
        );
        assert_eq!(
            hit_test(&layout, b_ov.x + 5.0, b_ov.y + 5.0),
            Hit::Floating { key: 8 }
        );

        // Switching virtual rows leaves floating rest frames untouched on screen.
        reanchor(&mut layout, 0, None);
        assert_eq!(layout.floating[0].rest, float_a);
        assert_eq!(layout.floating[1].rest, float_b);
    }

    #[test]
    fn many_floating_windows_shrink_proportionally_to_fit_shelf_width() {
        let floating: Vec<_> = (0..16_u32)
            .map(|i| FloatingInput {
                key: 100 + i,
                window_id: (100 + i).cast_signed(),
                frame: Rect::new(100.0, 100.0, 600.0, 300.0),
                focused: i == 0,
            })
            .collect();
        let metrics = Metrics::default();
        let layout = layout_display(SIZE, &strips(), &floating, 1.0, &metrics);
        assert_eq!(layout.floating.len(), 16);
        let first = &layout.floating[0].overview;
        let last = &layout.floating[15].overview;
        assert!(
            first.x >= metrics.ribbon_padding - 1e-6,
            "shelf overflowed left edge: {first:?}"
        );
        assert!(
            (last.x + last.w - (SIZE.0 - metrics.ribbon_padding)).abs() < 1e-6,
            "shelf right edge misaligned: {last:?}"
        );
        for pair in layout.floating.windows(2) {
            assert!(pair[0].overview.x + pair[0].overview.w < pair[1].overview.x);
            assert!((pair[0].overview.w / pair[0].overview.h - 2.0).abs() < 1e-6);
        }
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
    fn transitions_end_once_the_rest_is_too_small_to_see() {
        let spring = Spring::new(1.0, 0.0, 0.0, SPRING_RESPONSE);
        let shown = spring.visible_duration();
        assert!(shown < spring.settling_duration() - 0.05, "{shown}");
        assert!(spring.value(shown).abs() < VISIBLE_REMAINDER);
        assert!(spring.value(shown - 0.02).abs() >= VISIBLE_REMAINDER);
        // A transition going nowhere still gets a real duration.
        assert!(Spring::new(0.0, 0.0, 0.0, SPRING_RESPONSE).visible_duration() > 0.0);

        // High-velocity flicks cross `to` early and overshoot; `visible_duration`
        // must wait until the overshoot settles back within `VISIBLE_REMAINDER`.
        let flick = Spring::new(0.5, 1.0, 8.0, SPRING_RESPONSE);
        let shown = flick.visible_duration();
        let left = VISIBLE_REMAINDER * 0.5;
        assert!(
            shown > 0.2,
            "overshooting spring must not truncate at first crossing: {shown}"
        );
        for step in 0..120 {
            let t = shown + f64::from(step) / 240.0;
            assert!(
                (flick.value(t) - 1.0).abs() < left,
                "t={t}: {}",
                flick.value(t)
            );
        }
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
