use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{NonSendMut, Query, Res};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSBitmapImageRep, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSDeviceRGBColorSpace, NSFont, NSImage, NSImageScaling, NSImageView, NSLayoutAttribute, NSMenu,
    NSMenuItem, NSScreen, NSStackView, NSStatusBar, NSStatusItem, NSTextField,
    NSUserInterfaceLayoutOrientation, NSVariableStatusItemLength, NSView,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSInteger, NSObject, NSString};
use objc2_quartz_core::{CAGradientLayer, CALayer};
use tracing::warn;

use crate::accessibility_prompt::{AccessibilitySetupAction, show_accessibility_setup};
use crate::commands::{Command, Operation};
use crate::config::Config;
use crate::config::decorations::{
    DescriptorStyle, IndicatorFormat, IndicatorStyle, MenubarOrientation,
};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::ActiveDisplay;
use crate::ecs::{Bounds, FocusedMarker, Unmanaged};
use crate::events::{Event, EventSender};
use crate::manager::request_ax_privilege;
use crate::util::round_px;

#[derive(Debug, Clone)]
struct MenuActionTargetIvars {
    events: EventSender,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaneruMenuActionTarget"]
    #[ivars = MenuActionTargetIvars]
    #[derive(Debug)]
    struct MenuActionTarget;

    impl MenuActionTarget {
        #[unsafe(method(setWidth:))]
        fn set_width(&self, item: &NSMenuItem) {
            let Ok(percentage) = i32::try_from(item.tag()) else {
                return;
            };
            let ratio = f64::from(percentage) / 100.0;
            self.send_command(Command::Window(Operation::SetWidth(ratio)));
        }

        #[unsafe(method(centerWindow:))]
        fn center_window(&self, _: &NSMenuItem) {
            self.send_command(Command::Window(Operation::Center));
        }

        #[unsafe(method(toggleManaged:))]
        fn toggle_managed(&self, _: &NSMenuItem) {
            self.send_command(Command::Window(Operation::Manage));
        }

        #[unsafe(method(copyWindowRule:))]
        fn copy_window_rule(&self, _: &NSMenuItem) {
            self.send_command(Command::Window(Operation::CopyRule));
        }

        #[unsafe(method(openAccessibilitySettings:))]
        fn open_accessibility_settings(&self, _: &NSMenuItem) {
            if let Err(error) = std::process::Command::new("/usr/bin/open")
                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                .spawn()
            {
                warn!(%error, "unable to open Accessibility settings");
            }
        }

        #[unsafe(method(showAccessibilityInstructions:))]
        fn show_accessibility_instructions(&self, _: &NSMenuItem) {
            let Some(main_thread_marker) = MainThreadMarker::new() else {
                warn!("unable to show Accessibility instructions outside the main thread");
                return;
            };

            if show_accessibility_setup(main_thread_marker)
                == AccessibilitySetupAction::Continue
            {
                request_ax_privilege();
            }
        }

        #[unsafe(method(quitPaneru:))]
        fn quit_paneru(&self, _: &NSMenuItem) {
            self.send_command(Command::Quit);
        }
    }
);

impl MenuActionTarget {
    fn new(mtm: MainThreadMarker, events: EventSender) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuActionTargetIvars { events });
        unsafe { msg_send![super(this), init] }
    }

    fn send_command(&self, command: Command) {
        if let Err(error) = self.ivars().events.send(Event::Command { command }) {
            warn!(%error, "unable to send menu bar command");
        }
    }
}

const MENU_BAR_SPACING: CGFloat = 5.0;

pub struct MenuBarManager {
    mtm: MainThreadMarker,
    status_bar: Retained<NSStatusBar>,
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    action_target: Retained<MenuActionTarget>,
    width_items: Vec<(i32, Retained<NSMenuItem>)>,
    managed_window_items: Vec<Retained<NSMenuItem>>,
    manage_item: Option<Retained<NSMenuItem>>,
    copy_rule_item: Option<Retained<NSMenuItem>>,
    configured_widths: Vec<i32>,
    current_content: Option<MenuBarContent>,
}

#[derive(Debug, PartialEq, Eq)]
enum MenuBarContent {
    Text(String),
    Workspaces { current: Option<u32>, all: Vec<u32> },
}

#[derive(Debug, PartialEq)]
struct WindowMenuEnablement {
    managed_actions: bool,
    toggle_managed: bool,
}

fn window_menu_enablement(
    has_focused_window: bool,
    focused_width_ratio: Option<f64>,
) -> WindowMenuEnablement {
    WindowMenuEnablement {
        managed_actions: focused_width_ratio.is_some(),
        toggle_managed: has_focused_window,
    }
}

impl MenuBarManager {
    pub fn new(mtm: MainThreadMarker, events: EventSender) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        let action_target = MenuActionTarget::new(mtm, events);

        menu.setAutoenablesItems(false);
        status_item.setMenu(Some(&menu));
        status_item.setVisible(true);

        Self {
            mtm,
            status_bar,
            status_item,
            menu,
            action_target,
            width_items: Vec::new(),
            managed_window_items: Vec::new(),
            manage_item: None,
            copy_rule_item: None,
            configured_widths: Vec::new(),
            current_content: None,
        }
    }

    pub fn new_accessibility_required(mtm: MainThreadMarker, events: EventSender) -> Self {
        let mut manager = Self::new(mtm, events);
        manager.rebuild_accessibility_menu();
        manager.show_text("!");
        manager
    }

    fn rebuild_accessibility_menu(&mut self) {
        self.menu.removeAllItems();

        let status = self.add_item("Paneru — Accessibility Required", None);
        status.setEnabled(false);

        let hint = self.add_item("Grant access; Paneru will start automatically", None);
        hint.setEnabled(false);

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        self.add_item(
            "Show Setup Instructions…",
            Some(sel!(showAccessibilityInstructions:)),
        );
        self.add_item(
            "Open Accessibility Settings…",
            Some(sel!(openAccessibilitySettings:)),
        );

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        self.add_item("Quit Paneru", Some(sel!(quitPaneru:)));
    }

    pub fn update(
        &mut self,
        virtual_index: u32,
        virtual_indices: &[u32],
        config: &Config,
        has_focused_window: bool,
        focused_width_ratio: Option<f64>,
    ) {
        let preset_widths = config.preset_column_widths();
        let widths = normalized_width_percentages(&preset_widths);
        if self.configured_widths != widths {
            self.rebuild_menu(&widths);
        }

        let enablement = window_menu_enablement(has_focused_window, focused_width_ratio);
        for item in &self.managed_window_items {
            item.setEnabled(enablement.managed_actions);
        }
        if let Some(manage_item) = &self.manage_item {
            manage_item.setEnabled(enablement.toggle_managed);
        }
        if let Some(copy_rule_item) = &self.copy_rule_item {
            copy_rule_item.setEnabled(enablement.toggle_managed);
        }
        for (percentage, item) in &self.width_items {
            let selected = focused_width_ratio
                .is_some_and(|ratio| (ratio.mul_add(100.0, -f64::from(*percentage))).abs() < 1.0);
            item.setState(if selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }

        let current = config.workspace_menu_status().then_some(virtual_index);
        let all: &[u32] = if current.is_some() {
            virtual_indices
        } else {
            &[]
        };
        self.show_workspaces(config, current, all);
    }

    fn rebuild_menu(&mut self, widths: &[i32]) {
        self.menu.removeAllItems();
        self.width_items.clear();
        self.managed_window_items.clear();
        self.manage_item = None;
        self.copy_rule_item = None;

        let status = self.add_item("Paneru — Running", None);
        status.setEnabled(false);
        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));

        let width_header = self.add_item("Window width", None);
        width_header.setEnabled(false);
        for &percentage in widths {
            let item = self.add_item(&format!("{percentage}%"), Some(sel!(setWidth:)));
            item.setTag(isize::try_from(percentage).expect("width percentage fits in isize"));
            self.managed_window_items.push(item.clone());
            self.width_items.push((percentage, item));
        }

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        let center = self.add_item("Center Window", Some(sel!(centerWindow:)));
        let manage = self.add_item("Toggle Managed", Some(sel!(toggleManaged:)));
        self.managed_window_items.push(center);
        self.manage_item = Some(manage);

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        self.copy_rule_item = Some(self.add_item("Copy Window Rule", Some(sel!(copyWindowRule:))));

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        self.add_item("Quit Paneru", Some(sel!(quitPaneru:)));
        self.configured_widths = widths.to_vec();
    }

    fn add_item(&self, title: &str, action: Option<objc2::runtime::Sel>) -> Retained<NSMenuItem> {
        let item = unsafe {
            self.menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str(title),
                action,
                &NSString::from_str(""),
            )
        };
        if action.is_some() {
            unsafe { item.setTarget(Some(&self.action_target)) };
        }
        item
    }

    fn show_text(&mut self, label: &str) {
        let content = MenuBarContent::Text(label.to_owned());
        if self.current_content.as_ref() == Some(&content) {
            return;
        }

        let field = NSTextField::labelWithString(&NSString::from_str(label), self.mtm);
        field.setFont(Some(&NSFont::menuBarFontOfSize(0.0)));
        field.sizeToFit();
        let size = field.frame().size;

        if self.install(&field, size.width, size.height) {
            self.current_content = Some(content);
        }
    }

    fn show_workspaces(&mut self, config: &Config, current: Option<u32>, all: &[u32]) {
        let content = MenuBarContent::Workspaces {
            current,
            all: all.to_vec(),
        };
        if self.current_content.as_ref() == Some(&content) {
            return;
        }

        let descriptor = self.build_descriptor(config);
        let indicator = self.build_indicator(config, current, all);
        let ordered = match config.menubar_orientation() {
            MenubarOrientation::Default => [descriptor, indicator],
            MenubarOrientation::Flipped => [indicator, descriptor],
        };

        let stack = NSStackView::new(self.mtm);
        stack.setSpacing(MENU_BAR_SPACING);
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        for view in ordered.into_iter().flatten() {
            stack.addArrangedSubview(&view);
        }

        let fitting = stack.fittingSize();
        let size = CGSize::new(
            fitting.width,
            fitting.height.min(self.status_bar.thickness()),
        );

        let installed = match self.gradient_view(config, &stack, size) {
            Some(gradient) => self.install(&gradient, size.width, size.height),
            None => self.install(&stack, size.width, size.height),
        };
        if installed {
            self.current_content = Some(content);
        }
    }

    /// Wraps `content` in a view hosting a [`CAGradientLayer`] masked by a
    /// bitmap render of `content`, so the configured colours fill the glyphs
    /// themselves rather than the box around them.
    ///
    /// Returns `None` when there is no gradient to draw or any step of the
    /// render fails, leaving the caller to install `content` untouched.
    fn gradient_view(
        &self,
        config: &Config,
        content: &NSView,
        size: CGSize,
    ) -> Option<Retained<NSView>> {
        let gradient = config.menubar_gradient();
        if gradient.is_empty() || size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        let bounds = CGRect::new(CGPoint::ZERO, size);
        let scale = self.backing_scale_factor();

        let rep = render_content_bitmap(content, bounds, scale)?;
        let image = rep.CGImage()?;
        let mask = CALayer::new();
        mask.setFrame(bounds);
        mask.setContentsScale(scale);
        unsafe { mask.setContents(Some(AsRef::<AnyObject>::as_ref(&*image))) };

        let colors = gradient
            .into_iter()
            .map(|(red, green, blue)| {
                NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, 1.0).CGColor()
            })
            .collect::<Vec<_>>();
        let color_objects = colors
            .iter()
            .map(|color| AsRef::<AnyObject>::as_ref(&**color))
            .collect::<Vec<_>>();

        let angle = config.menubar_gradient_angle();
        let layer = CAGradientLayer::new();
        layer.setFrame(bounds);
        layer.setContentsScale(scale);
        layer.setStartPoint(unit_point_for_angle(size, angle + 180.0));
        layer.setEndPoint(unit_point_for_angle(size, angle));
        unsafe {
            layer.setColors(Some(&NSArray::from_slice(&color_objects)));
            layer.setMask(Some(&mask));
        }

        let view = NSView::new(self.mtm);
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
        Some(view)
    }

    /// The scale the mask bitmap has to be rendered at. It cannot be read back
    /// from the content view, which is still outside any window while it is
    /// being rendered.
    fn backing_scale_factor(&self) -> CGFloat {
        self.status_item
            .button(self.mtm)
            .and_then(|button| button.window())
            .map(|window| window.backingScaleFactor())
            .or_else(|| NSScreen::mainScreen(self.mtm).map(|screen| screen.backingScaleFactor()))
            .filter(|scale| *scale > 0.0)
            .unwrap_or(1.0)
    }

    fn install(&self, view: &NSView, width: CGFloat, height: CGFloat) -> bool {
        let Some(button) = self.status_item.button(self.mtm) else {
            warn!("unable to update menu bar: status item has no button");
            return false;
        };

        for subview in button.subviews() {
            subview.removeFromSuperview();
        }

        if width <= 0.0 {
            self.status_item.setLength(0.0);
            return true;
        }

        let origin_y = ((self.status_bar.thickness() - height) / 2.0).max(0.0);
        view.setTranslatesAutoresizingMaskIntoConstraints(true);
        view.setFrame(CGRect::new(
            CGPoint::new(MENU_BAR_SPACING, origin_y),
            CGSize::new(width, height),
        ));
        button.addSubview(view);
        button.setToolTip(Some(&NSString::from_str("Paneru window manager")));
        self.status_item.setLength(width);
        true
    }

    fn build_indicator(
        &self,
        config: &Config,
        current: Option<u32>,
        all: &[u32],
    ) -> Option<Retained<NSStackView>> {
        let current = current?;
        let style = config.menubar_indicator_style();
        let format = match (style, config.menubar_indicator_format()) {
            (IndicatorStyle::Mono, IndicatorFormat::Unicode) => IndicatorFormat::Default,
            (_, format) => format,
        };

        let stack = NSStackView::new(self.mtm);
        stack.setSpacing(MENU_BAR_SPACING);
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        stack.setAlignment(NSLayoutAttribute::CenterY);
        match style {
            IndicatorStyle::Mono => {
                stack.addArrangedSubview(&self.format_indicator(config, format, current, false));
            }
            IndicatorStyle::Multi => {
                for &virtual_index in all {
                    let field = self.format_indicator(
                        config,
                        format,
                        virtual_index,
                        virtual_index == current,
                    );
                    stack.addArrangedSubview(&field);
                }
            }
        }
        Some(stack)
    }

    fn format_indicator(
        &self,
        config: &Config,
        format: IndicatorFormat,
        virtual_index: u32,
        is_active: bool,
    ) -> Retained<NSTextField> {
        let label = match format {
            IndicatorFormat::Default => virtual_workspace_label(virtual_index),
            IndicatorFormat::Roman => roman_numeral(virtual_index.saturating_add(1)),
            IndicatorFormat::Unicode => if is_active {
                config.menubar_indicator_active_character()
            } else {
                config.menubar_indicator_inactive_character()
            }
            .to_string(),
        };
        let font_size = config.menubar_indicator_font_size();
        let font = if is_active && format != IndicatorFormat::Unicode {
            NSFont::boldSystemFontOfSize(font_size)
        } else {
            NSFont::systemFontOfSize(font_size)
        };
        let field = NSTextField::labelWithString(&NSString::from_str(&label), self.mtm);
        field.setFont(Some(&font));
        field
    }

    fn build_descriptor(&self, config: &Config) -> Option<Retained<NSStackView>> {
        let style = config.menubar_descriptor_style();
        if style == DescriptorStyle::Hidden {
            return None;
        }
        let stack = NSStackView::new(self.mtm);
        stack.setSpacing(MENU_BAR_SPACING);
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        stack.setAlignment(NSLayoutAttribute::CenterY);
        if matches!(style, DescriptorStyle::Symbol | DescriptorStyle::Both)
            && let Some(image_view) = self.build_descriptor_symbol(config)
        {
            stack.addArrangedSubview(&image_view);
        }
        if matches!(style, DescriptorStyle::Text | DescriptorStyle::Both) {
            stack.addArrangedSubview(&self.build_descriptor_text(config));
        }
        (!stack.arrangedSubviews().is_empty()).then_some(stack)
    }

    fn build_descriptor_symbol(&self, config: &Config) -> Option<Retained<NSImageView>> {
        let symbol = config.menubar_descriptor_symbol();
        let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(&symbol),
            None,
        ) else {
            warn!(%symbol, "unable to load menu bar descriptor symbol");
            return None;
        };
        image.setTemplate(true);

        let size = image.size();
        let aspect_ratio = if size.height > 0.0 {
            size.width / size.height
        } else {
            1.0
        };
        let scaled = CGSize::new(14.0 * aspect_ratio, 14.0);
        image.setSize(scaled);

        let image_view = NSImageView::new(self.mtm);
        image_view.setImage(Some(&image));
        image_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        image_view.setFrameSize(scaled);
        image_view.setContentTintColor(Some(&*NSColor::whiteColor()));
        Some(image_view)
    }

    fn build_descriptor_text(&self, config: &Config) -> Retained<NSTextField> {
        let field = NSTextField::labelWithString(
            &NSString::from_str(&config.menubar_descriptor_text()),
            self.mtm,
        );
        field.setFont(Some(&NSFont::menuBarFontOfSize(0.0)));
        field
    }
}

impl Drop for MenuBarManager {
    fn drop(&mut self) {
        self.status_bar.removeStatusItem(&self.status_item);
    }
}

pub fn update_menu_bar(
    active_display: ActiveDisplay,
    workspaces: Query<&LayoutStrip>,
    focused: Query<(&Bounds, Has<Unmanaged>), With<FocusedMarker>>,
    config: Res<Config>,
    menu_bar: Option<NonSendMut<MenuBarManager>>,
) {
    let Some(mut menu_bar) = menu_bar else {
        return;
    };
    let strip = active_display.active_strip();
    let mut virtual_indices = workspaces
        .iter()
        .filter(|workspace| workspace.id() == strip.id())
        .map(|workspace| workspace.virtual_index)
        .collect::<Vec<_>>();
    virtual_indices.sort_unstable();
    virtual_indices.dedup();
    let viewport = active_display.actual_bounds(&config);

    let focused_window = focused.iter().next();
    let focused_width_ratio = focused_window.and_then(|(bounds, unmanaged)| {
        (!unmanaged).then(|| f64::from(bounds.0.x) / f64::from(viewport.width()))
    });

    menu_bar.update(
        strip.virtual_index,
        &virtual_indices,
        &config,
        focused_window.is_some(),
        focused_width_ratio,
    );
}

pub(crate) fn virtual_workspace_label(virtual_index: u32) -> String {
    (virtual_index + 1).to_string()
}

fn roman_numeral(value: u32) -> String {
    const NUMERALS: [(u32, &str); 7] = [
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];

    let mut remaining = value;
    let mut numeral = String::new();
    while remaining > 0 {
        let Some(&(weight, symbol)) = NUMERALS.iter().find(|(weight, _)| *weight <= remaining)
        else {
            break;
        };
        remaining -= weight;
        numeral.push_str(symbol);
    }
    numeral
}

/// Renders `content` into a bitmap at `scale`, for use as a [`CALayer`] mask:
/// only the alpha channel is read back, so the colors `AppKit` happens to draw
/// the labels in do not matter.
fn render_content_bitmap(
    content: &NSView,
    bounds: CGRect,
    scale: CGFloat,
) -> Option<Retained<NSBitmapImageRep>> {
    let pixels_wide = NSInteger::try_from(round_px(bounds.size.width * scale)).ok()?;
    let pixels_high = NSInteger::try_from(round_px(bounds.size.height * scale)).ok()?;

    content.setFrame(bounds);
    content.layoutSubtreeIfNeeded();

    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pixels_wide,
            pixels_high,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    rep.setSize(bounds.size);
    content.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    Some(rep)
}

/// The point where a ray leaving the center of a `size`-sized rectangle at
/// `degrees` crosses that rectangle's edge, in the unit coordinate space
/// [`CAGradientLayer`] takes its start and end points in.
///
/// Angles run counter-clockwise from 0° pointing right, matching the layer's
/// unflipped geometry: 90° is the top edge, 270° the bottom one.
fn unit_point_for_angle(size: CGSize, degrees: f64) -> CGPoint {
    let center = CGPoint::new(0.5, 0.5);
    if size.width <= 0.0 || size.height <= 0.0 {
        return center;
    }

    let (sin, cos) = degrees.to_radians().sin_cos();

    let to_side = (size.width / 2.0) / cos.abs();
    let to_cap = (size.height / 2.0) / sin.abs();
    let distance = to_side.min(to_cap);

    CGPoint::new(
        center.x + distance * cos / size.width,
        center.y + distance * sin / size.height,
    )
}

fn normalized_width_percentages(widths: &[f64]) -> Vec<i32> {
    let mut percentages = widths
        .iter()
        .copied()
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
        .map(|ratio| round_px(ratio.mul_add(100.0, 0.0)))
        .filter(|percentage| *percentage > 0)
        .collect::<Vec<_>>();
    percentages.sort_unstable();
    percentages.dedup();
    percentages
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::{
        WindowMenuEnablement, normalized_width_percentages, roman_numeral, unit_point_for_angle,
        virtual_workspace_label, window_menu_enablement,
    };

    const EPSILON: f64 = 1e-9;

    fn assert_unit_point(point: CGPoint, x: f64, y: f64) {
        assert!(
            (point.x - x).abs() < EPSILON && (point.y - y).abs() < EPSILON,
            "expected ({x}, {y}), got ({}, {})",
            point.x,
            point.y
        );
    }

    #[test]
    fn virtual_workspace_label_is_one_based() {
        assert_eq!(virtual_workspace_label(0), "1");
        assert_eq!(virtual_workspace_label(4), "5");
    }

    #[test]
    fn roman_numerals_terminate_and_subtract_correctly() {
        assert_eq!(roman_numeral(0), "");
        assert_eq!(roman_numeral(1), "I");
        assert_eq!(roman_numeral(3), "III");
        assert_eq!(roman_numeral(4), "IV");
        assert_eq!(roman_numeral(5), "V");
        assert_eq!(roman_numeral(9), "IX");
        assert_eq!(roman_numeral(10), "X");
        assert_eq!(roman_numeral(40), "XL");
        assert_eq!(roman_numeral(49), "XLIX");
        assert_eq!(roman_numeral(50), "L");
        assert_eq!(roman_numeral(89), "LXXXIX");
    }

    #[test]
    fn menu_widths_are_sorted_deduplicated_and_valid() {
        assert_eq!(
            normalized_width_percentages(&[2.0, 0.5, 1.5, 0.5, 0.001, f64::NAN, -1.0]),
            vec![50, 150, 200]
        );
    }

    #[test]
    fn cardinal_gradient_angles_hit_edge_midpoints() {
        let size = CGSize::new(100.0, 20.0);
        assert_unit_point(unit_point_for_angle(size, 0.0), 1.0, 0.5);
        assert_unit_point(unit_point_for_angle(size, 90.0), 0.5, 1.0);
        assert_unit_point(unit_point_for_angle(size, 180.0), 0.0, 0.5);
        assert_unit_point(unit_point_for_angle(size, 270.0), 0.5, 0.0);
    }

    #[test]
    fn gradient_angles_leave_through_the_nearer_edge() {
        // 45° out of a wide, short rect reaches the top long before the side,
        // and out of a square it lands exactly on the corner.
        assert_unit_point(
            unit_point_for_angle(CGSize::new(100.0, 20.0), 45.0),
            0.6,
            1.0,
        );
        assert_unit_point(
            unit_point_for_angle(CGSize::new(40.0, 40.0), 45.0),
            1.0,
            1.0,
        );
    }

    #[test]
    fn gradient_angles_wrap_and_stay_opposite() {
        let size = CGSize::new(64.0, 22.0);
        assert_unit_point(unit_point_for_angle(size, 450.0), 0.5, 1.0);
        assert_unit_point(unit_point_for_angle(size, -90.0), 0.5, 0.0);

        // What the caller relies on: the start and end of a gradient sit on
        // opposite sides of the centre.
        let start = unit_point_for_angle(size, 30.0 + 180.0);
        let end = unit_point_for_angle(size, 30.0);
        assert!((start.x + end.x - 1.0).abs() < EPSILON);
        assert!((start.y + end.y - 1.0).abs() < EPSILON);
    }

    #[test]
    fn gradient_angles_on_a_degenerate_rect_stay_centred() {
        assert_unit_point(unit_point_for_angle(CGSize::new(0.0, 20.0), 45.0), 0.5, 0.5);
        assert_unit_point(
            unit_point_for_angle(CGSize::new(100.0, 0.0), 45.0),
            0.5,
            0.5,
        );
    }

    #[test]
    fn unmanaged_focus_only_enables_toggle_managed() {
        assert_eq!(
            window_menu_enablement(true, None),
            WindowMenuEnablement {
                managed_actions: false,
                toggle_managed: true,
            }
        );
        assert_eq!(
            window_menu_enablement(false, None),
            WindowMenuEnablement {
                managed_actions: false,
                toggle_managed: false,
            }
        );
        assert_eq!(
            window_menu_enablement(true, Some(1.0)),
            WindowMenuEnablement {
                managed_actions: true,
                toggle_managed: true,
            }
        );
    }
}
