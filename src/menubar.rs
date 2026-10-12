use bevy::ecs::change_detection::{DetectChanges, DetectChangesMut};
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Added, Changed, Has, Or, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{NonSendMut, Query, Res, ResMut};
use objc2::rc::Retained;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageRep, NSCellImagePosition, NSColor, NSCompositingOperation,
    NSControlStateValueOff, NSControlStateValueOn, NSDeviceRGBColorSpace, NSFont, NSGradient,
    NSGraphicsContext, NSImage, NSImageScaling, NSImageView, NSLayoutAttribute, NSMenu, NSMenuItem,
    NSScreen, NSStackView, NSStatusBar, NSStatusItem, NSTextField,
    NSUserInterfaceLayoutOrientation, NSVariableStatusItemLength, NSView,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSArray, NSInteger, NSObject, NSString};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

use crate::accessibility_prompt::{
    AccessibilitySetupAction, handle_accessibility_setup_action, permission_pane_name,
    show_accessibility_setup,
};
use crate::commands::{Command, Operation};
use crate::config::Config;
use crate::config::decorations::{
    DescriptorStyle, IndicatorFormat, IndicatorStyle, MenubarOrientation,
};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::ActiveDisplay;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, Unmanaged,
};
use crate::events::{Event, EventSender};
use crate::manager::Display;
use crate::util::round_px;

/// A menu or command choice overrides the configuration until changed again.
#[derive(Debug, Default, Resource, Serialize, Deserialize)]
pub struct MenuBarVisibility {
    enabled: Option<bool>,
    #[serde(skip)]
    dirty: bool,
}

impl MenuBarVisibility {
    pub fn enabled(&self, config: &Config) -> bool {
        self.enabled.unwrap_or_else(|| config.menubar_enabled())
    }

    fn load(path: &Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                warn!(%error, path = %path.display(), "unable to read menu bar preference");
                return Self::default();
            }
        };
        serde_json::from_str(&data).unwrap_or_else(|error| {
            warn!(%error, path = %path.display(), "ignoring invalid menu bar preference");
            Self::default()
        })
    }

    fn save(&self, path: &Path) -> crate::errors::Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, data)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

#[derive(Resource)]
pub struct MenuBarPreferenceFile(PathBuf);

pub fn load_menu_bar_preference(app: &mut bevy::app::App) {
    if let Some(path) = xdg::BaseDirectories::with_prefix("paneru").get_state_file("menu-bar.json")
    {
        app.insert_resource(MenuBarVisibility::load(&path))
            .insert_resource(MenuBarPreferenceFile(path));
    }
}

pub fn setup_menu_bar(
    app: &mut bevy::app::App,
    mtm: MainThreadMarker,
    events: EventSender,
    initial_config: &Config,
) {
    // Prefer Lua setup when present; otherwise reuse the configuration already
    // loaded for the event tap rather than reading it again.
    let config = app
        .world()
        .get_resource::<Config>()
        .unwrap_or(initial_config);
    let visible = app.world().resource::<MenuBarVisibility>().enabled(config);
    app.insert_non_send(MenuBarManager::new(mtm, events, visible));
}

pub fn menu_bar_command_handler(
    mut messages: MessageReader<Event>,
    mut visibility: ResMut<MenuBarVisibility>,
) {
    for event in messages.read() {
        if let Event::Command {
            command: Command::MenuBar(enabled),
        } = event
            && visibility.enabled != Some(*enabled)
        {
            visibility.enabled = Some(*enabled);
            visibility.dirty = true;
        }
    }
}

pub fn save_menu_bar_preference(
    mut visibility: ResMut<MenuBarVisibility>,
    file: Option<Res<MenuBarPreferenceFile>>,
) {
    if !visibility.dirty {
        return;
    }
    let Some(file) = file else {
        return;
    };
    match visibility.save(&file.0) {
        Ok(()) => visibility.bypass_change_detection().dirty = false,
        Err(error) => {
            warn!(%error, path = %file.0.display(), "unable to save menu bar preference");
        }
    }
}

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
                let pane = permission_pane_name();
                warn!(%error, pane, "unable to open permission settings");
            }
        }

        #[unsafe(method(showAccessibilityInstructions:))]
        fn show_accessibility_instructions(&self, _: &NSMenuItem) {
            let Some(main_thread_marker) = MainThreadMarker::new() else {
                warn!("unable to show setup instructions outside the main thread");
                return;
            };

            let action = show_accessibility_setup(main_thread_marker);
            handle_accessibility_setup_action(action);
        }

        #[unsafe(method(resetAccessibilityPermission:))]
        fn reset_accessibility_permission(&self, _: &NSMenuItem) {
            handle_accessibility_setup_action(AccessibilitySetupAction::ResetAndRetry);
        }

        #[unsafe(method(quitPaneru:))]
        fn quit_paneru(&self, _: &NSMenuItem) {
            self.send_command(Command::Quit);
        }

        #[unsafe(method(hideMenuBar:))]
        fn hide_menu_bar(&self, _: &NSMenuItem) {
            self.send_command(Command::MenuBar(false));
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
    visible: bool,
    current_enablement: Option<WindowMenuEnablement>,
    selected_widths: Option<Vec<i32>>,
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
    fn set_visible(&mut self, visible: bool) {
        if self.visible != visible {
            self.status_item.setVisible(visible);
            self.visible = visible;
        }
    }

    pub fn new(mtm: MainThreadMarker, events: EventSender, visible: bool) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        let action_target = MenuActionTarget::new(mtm, events);

        menu.setAutoenablesItems(false);
        status_item.setMenu(Some(&menu));
        // AppKit creates a visible item, so the default needs no transaction.
        if !visible {
            status_item.setVisible(false);
        }

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
            visible,
            current_enablement: None,
            selected_widths: None,
        }
    }

    pub fn new_accessibility_required(mtm: MainThreadMarker, events: EventSender) -> Self {
        let mut manager = Self::new(mtm, events, true);
        manager.rebuild_accessibility_menu();
        manager.show_text("!");
        manager
    }

    fn rebuild_accessibility_menu(&mut self) {
        let pane = permission_pane_name();
        self.menu.removeAllItems();

        let status = self.add_item(&format!("Paneru — {pane} Required"), None);
        status.setEnabled(false);

        let hint = self.add_item("Grant access; Paneru will start automatically", None);
        hint.setEnabled(false);

        self.menu.addItem(&NSMenuItem::separatorItem(self.mtm));
        self.add_item(
            "Show Setup Instructions…",
            Some(sel!(showAccessibilityInstructions:)),
        );
        self.add_item(
            &format!("Open {pane} Settings…"),
            Some(sel!(openAccessibilitySettings:)),
        );
        self.add_item(
            "Reset Permission & Retry…",
            Some(sel!(resetAccessibilityPermission:)),
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
        if self.configured_widths != widths || self.manage_item.is_none() {
            self.rebuild_menu(&widths);
        }

        let enablement = window_menu_enablement(has_focused_window, focused_width_ratio);
        if self.current_enablement.as_ref() != Some(&enablement) {
            for item in &self.managed_window_items {
                item.setEnabled(enablement.managed_actions);
            }
            if let Some(manage_item) = &self.manage_item {
                manage_item.setEnabled(enablement.toggle_managed);
            }
            if let Some(copy_rule_item) = &self.copy_rule_item {
                copy_rule_item.setEnabled(enablement.toggle_managed);
            }
            self.current_enablement = Some(enablement);
        }
        let selected_widths = selected_width_percentages(&widths, focused_width_ratio);
        if self.selected_widths.as_ref() != Some(&selected_widths) {
            for (percentage, item) in &self.width_items {
                item.setState(if selected_widths.contains(percentage) {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
            self.selected_widths = Some(selected_widths);
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
        let hide = self.add_item("Hide Menu Bar", Some(sel!(hideMenuBar:)));
        hide.setToolTip(Some(&NSString::from_str(
            "Show again with: paneru showmenu",
        )));
        self.add_item("Quit Paneru", Some(sel!(quitPaneru:)));
        self.configured_widths = widths.to_vec();
        self.current_enablement = None;
        self.selected_widths = None;
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

        if self.install(self.content_image(&field, size).as_deref()) {
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

        let image = self.content_image(&stack, size).map(|image| {
            self.colorized(config, &image, size)
                .unwrap_or_else(|| image.clone())
        });
        if self.install(image.as_deref()) {
            self.current_content = Some(content);
        }
    }

    /// Bakes `content` into the image the status item button draws.
    ///
    /// The button gets an image rather than `content` itself as a subview:
    /// `AppKit` snapshots a status item's button into its menu bar replicants,
    /// and a live subview - auto layout, or a hosted layer - re-dirties itself
    /// on every snapshot, so the two feed each other in a loop that pins the
    /// main thread at ~75% of a core and starves the run loop the event tap is
    /// serviced on: the tap times out, macOS switches it off and every
    /// keybinding falls through to whatever app is focused. A flat image is
    /// snapshotted once and never invalidates itself.
    ///
    /// The image is a template, so `AppKit` tints it for the current menu bar
    /// appearance - light or dark, and inverted while the menu is open - which
    /// only the alpha channel of the render takes part in. [`colorized`]
    /// replaces that with the configured colours where there are any.
    ///
    /// [`colorized`]: Self::colorized
    fn content_image(&self, content: &NSView, size: CGSize) -> Option<Retained<NSImage>> {
        if size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        let bounds = CGRect::new(CGPoint::ZERO, size);
        let rep = render_content_bitmap(content, bounds, self.backing_scale_factor())?;

        let image = NSImage::initWithSize(NSImage::alloc(), size);
        image.addRepresentation(&rep);
        image.setTemplate(true);
        Some(image)
    }

    /// Fills the glyphs of `content` with the configured colours, so they
    /// carry the gradient themselves rather than the box around them.
    ///
    /// Returns `None` when no colours are configured, or when the render fails
    /// at any step: the template image the caller already has follows the menu
    /// bar's own appearance, which a baked colour would override in one of the
    /// two themes anyway.
    fn colorized(
        &self,
        config: &Config,
        content: &NSImage,
        size: CGSize,
    ) -> Option<Retained<NSImage>> {
        let colors = config
            .menubar_gradient()
            .into_iter()
            .map(|(red, green, blue)| {
                NSColor::colorWithSRGBRed_green_blue_alpha(red, green, blue, 1.0)
            })
            .collect::<Vec<_>>();
        if colors.is_empty() {
            return None;
        }

        let bounds = CGRect::new(CGPoint::ZERO, size);
        let rep = bitmap_rep(bounds, self.backing_scale_factor())?;
        let canvas = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;

        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&canvas));
        let color_refs = colors.iter().map(|color| &**color).collect::<Vec<_>>();
        // A single colour is a gradient with nowhere to go, and `NSGradient`
        // refuses to be built from one.
        if let Some(gradient) =
            NSGradient::initWithColors(NSGradient::alloc(), &NSArray::from_slice(&color_refs))
        {
            gradient.drawInRect_angle(bounds, config.menubar_gradient_angle());
        } else {
            colors[0].setFill();
            NSBezierPath::fillRect(bounds);
        }
        // Keeps the fill only where the content drew, punching the glyphs out
        // of the colour rather than painting a coloured box behind them.
        content.drawInRect_fromRect_operation_fraction(
            bounds,
            CGRect::ZERO,
            NSCompositingOperation::DestinationIn,
            1.0,
        );
        NSGraphicsContext::restoreGraphicsState_class();

        let image = NSImage::initWithSize(NSImage::alloc(), size);
        image.addRepresentation(&rep);
        Some(image)
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

    /// Draws `image` in the status item, or empties the item when there is
    /// nothing to draw.
    ///
    /// The item keeps [`NSVariableStatusItemLength`]: `AppKit` sizes it around
    /// the image, padding included, which is what the item's own content
    /// insets are for - measuring the content and setting the length by hand
    /// left it off-centre by exactly one padding.
    fn install(&self, image: Option<&NSImage>) -> bool {
        let Some(button) = self.status_item.button(self.mtm) else {
            warn!("unable to update menu bar: status item has no button");
            return false;
        };

        let Some(image) = image else {
            button.setImage(None);
            self.status_item.setLength(0.0);
            return true;
        };

        button.setImage(Some(image));
        // A button starts out at `NSNoImage`, which draws its (empty) title and
        // nothing else: the item goes blank without this.
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        button.setToolTip(Some(&NSString::from_str("Paneru window manager")));
        self.status_item.setLength(NSVariableStatusItemLength);
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
        // Both formats that distinguish the active workspace from the rest say
        // nothing on their own, so a single-item indicator - `mono`, `paged` -
        // falls back to numbers.
        let format = match (style, config.menubar_indicator_format()) {
            (
                IndicatorStyle::Mono | IndicatorStyle::Paged,
                IndicatorFormat::Unicode | IndicatorFormat::Marked,
            ) => IndicatorFormat::Default,
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
            IndicatorStyle::Paged => {
                let last = paged_last_index(all.len());
                stack.addArrangedSubview(&self.format_indicator(config, format, current, true));
                stack.addArrangedSubview(&self.indicator_field(config, "/", false));
                stack.addArrangedSubview(&self.format_indicator(config, format, last, false));
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
        let label = indicator_label(
            format,
            virtual_index,
            is_active,
            config.menubar_indicator_active_character(),
            config.menubar_indicator_inactive_character(),
        );
        self.indicator_field(
            config,
            &label,
            is_active && format != IndicatorFormat::Unicode,
        )
    }

    /// One label of the indicator, bold when it stands for the active
    /// workspace.
    fn indicator_field(&self, config: &Config, label: &str, bold: bool) -> Retained<NSTextField> {
        let font_size = config.menubar_indicator_font_size();
        let font = if bold {
            NSFont::boldSystemFontOfSize(font_size)
        } else {
            NSFont::systemFontOfSize(font_size)
        };
        let field = NSTextField::labelWithString(&NSString::from_str(label), self.mtm);
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
        if config.menubar_has_external_gradient() {
            image_view.setContentTintColor(Some(&*NSColor::whiteColor()));
        }
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

type FocusedMenuChanges = (
    With<FocusedMarker>,
    Or<(Added<FocusedMarker>, Changed<Bounds>, Changed<Unmanaged>)>,
);

/// Position-only animation and swipe frames do not affect menu content.
pub(crate) fn menu_bar_dirty(
    strip_changed: Query<(), (With<ActiveWorkspaceMarker>, Changed<LayoutStrip>)>,
    focus_changed: Query<(), FocusedMenuChanges>,
    workspace_changed: Query<(), Added<ActiveWorkspaceMarker>>,
    strips_added: Query<(), Added<LayoutStrip>>,
    mut focus_lost: RemovedComponents<FocusedMarker>,
    mut unmanaged_removed: RemovedComponents<Unmanaged>,
    mut strips_removed: RemovedComponents<LayoutStrip>,
) -> bool {
    // Drain every removal reader even when another input already dirtied us.
    let focus_lost = focus_lost.read().count() > 0;
    let unmanaged_removed = unmanaged_removed.read().count() > 0;
    let strips_removed = strips_removed.read().count() > 0;
    !strip_changed.is_empty()
        || !focus_changed.is_empty()
        || !workspace_changed.is_empty()
        || !strips_added.is_empty()
        || focus_lost
        || unmanaged_removed
        || strips_removed
}

type MenuViewportChanges = (
    With<ActiveDisplayMarker>,
    Or<(
        Added<ActiveDisplayMarker>,
        Changed<Display>,
        Changed<DockPosition>,
    )>,
);

pub(crate) fn menu_viewport_dirty(
    display_changed: Query<(), MenuViewportChanges>,
    mut dock_removed: RemovedComponents<DockPosition>,
) -> bool {
    let dock_removed = dock_removed.read().count() > 0;
    !display_changed.is_empty() || dock_removed
}

fn selected_width_percentages(widths: &[i32], ratio: Option<f64>) -> Vec<i32> {
    widths
        .iter()
        .copied()
        .filter(|percentage| {
            ratio.is_some_and(|ratio| ratio.mul_add(100.0, -f64::from(*percentage)).abs() < 1.0)
        })
        .collect()
}

pub fn update_menu_bar(
    active_display: ActiveDisplay,
    workspaces: Query<&LayoutStrip>,
    focused: Query<(&Bounds, Has<Unmanaged>), With<FocusedMarker>>,
    config: Res<Config>,
    visibility: Res<MenuBarVisibility>,
    menu_bar: Option<NonSendMut<MenuBarManager>>,
) {
    let Some(mut menu_bar) = menu_bar else {
        return;
    };
    if config.is_changed() {
        menu_bar.current_content = None;
    }
    let enabled = visibility.enabled(&config);
    menu_bar.set_visible(enabled);
    if !enabled {
        return;
    }
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

/// What a single indicator position reads as.
fn indicator_label(
    format: IndicatorFormat,
    virtual_index: u32,
    is_active: bool,
    active_character: char,
    inactive_character: char,
) -> String {
    match format {
        IndicatorFormat::Default => virtual_workspace_label(virtual_index),
        IndicatorFormat::Roman => roman_numeral(virtual_index.saturating_add(1)),
        IndicatorFormat::Unicode => if is_active {
            active_character
        } else {
            inactive_character
        }
        .to_string(),
        IndicatorFormat::Marked if is_active => virtual_workspace_label(virtual_index),
        IndicatorFormat::Marked => inactive_character.to_string(),
    }
}

/// The index the paged indicator's total is formatted from: the workspace
/// count reads as the last workspace's own label, so it goes through the same
/// formatter and comes out Roman when the format asks for it.
fn paged_last_index(count: usize) -> u32 {
    u32::try_from(count.max(1) - 1).unwrap_or(u32::MAX)
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

/// Renders `content` into a bitmap at `scale`, the flat stand-in the status
/// item button draws instead of `content` itself.
fn render_content_bitmap(
    content: &NSView,
    bounds: CGRect,
    scale: CGFloat,
) -> Option<Retained<NSBitmapImageRep>> {
    content.setFrame(bounds);
    content.layoutSubtreeIfNeeded();

    let rep = bitmap_rep(bounds, scale)?;
    content.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    Some(rep)
}

/// An empty `bounds`-sized bitmap to draw into, backed by `scale` pixels per
/// point so the render survives a Retina menu bar.
fn bitmap_rep(bounds: CGRect, scale: CGFloat) -> Option<Retained<NSBitmapImageRep>> {
    let pixels_wide = NSInteger::try_from(round_px(bounds.size.width * scale)).ok()?;
    let pixels_high = NSInteger::try_from(round_px(bounds.size.height * scale)).ok()?;

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
    Some(rep)
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
    use super::{
        Command, Config, Event, IndicatorFormat, MenuBarPreferenceFile, MenuBarVisibility,
        WindowMenuEnablement, indicator_label, normalized_width_percentages, paged_last_index,
        roman_numeral, virtual_workspace_label, window_menu_enablement,
    };

    #[test]
    fn menu_bar_choice_survives_workspace_changes_and_restarts() {
        use crate::commands::{Direction, Operation};
        use crate::tests::TestHarness;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "paneru-menu-preference-{}-{stamp}.json",
            std::process::id()
        ));
        let hidden_path = path.clone();
        let shown_path = path.clone();
        let mut harness = TestHarness::new().with_windows(2);
        harness
            .app
            .insert_resource(MenuBarPreferenceFile(path.clone()));
        harness
            .on_iteration(0, move |world, _| {
                let config = world.resource::<Config>();
                assert!(!world.resource::<MenuBarVisibility>().enabled(config));
                assert!(!MenuBarVisibility::load(&hidden_path).enabled(config));
            })
            .on_iteration(1, |world, _| {
                assert!(
                    !world
                        .resource::<MenuBarVisibility>()
                        .enabled(world.resource::<Config>())
                );
            })
            .on_iteration(2, move |world, _| {
                let config = world.resource::<Config>();
                assert!(world.resource::<MenuBarVisibility>().enabled(config));
                assert!(MenuBarVisibility::load(&shown_path).enabled(config));
            })
            .run(vec![
                Event::Command {
                    command: Command::MenuBar(false),
                },
                Event::Command {
                    command: Command::Window(Operation::Focus(Direction::East)),
                },
                Event::Command {
                    command: Command::MenuBar(true),
                },
            ]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_or_invalid_menu_bar_preference_uses_config() {
        let config =
            Config::try_from("[options]\n[bindings]\n[decorations.menu]\nenabled = false").unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("paneru-menu-fallback-{stamp}.json"));
        assert!(!MenuBarVisibility::load(&path).enabled(&config));
        MenuBarVisibility {
            enabled: Some(true),
            ..MenuBarVisibility::default()
        }
        .save(&path)
        .unwrap();
        assert!(MenuBarVisibility::load(&path).enabled(&config));
        std::fs::write(&path, "invalid JSON").unwrap();
        assert!(!MenuBarVisibility::load(&path).enabled(&config));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn loaded_menu_preference_is_not_rewritten_until_a_new_choice() {
        use crate::tests::TestHarness;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("paneru-menu-loaded-{stamp}.json"));
        let original = "{ \"enabled\": false }\n";
        std::fs::write(&path, original).unwrap();
        let mut harness = TestHarness::new();
        harness
            .app
            .insert_resource(MenuBarVisibility::load(&path))
            .insert_resource(MenuBarPreferenceFile(path.clone()));
        harness.app.update();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_extension("json.tmp").exists());

        harness.world().write_message(Event::Command {
            command: Command::MenuBar(false),
        });
        harness.app.update();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        harness.world().write_message(Event::Command {
            command: Command::MenuBar(true),
        });
        harness.app.update();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(MenuBarVisibility::load(&path).enabled(harness.world().resource::<Config>()));
        assert!(!saved.contains("dirty"));
        assert!(!harness.world().resource::<MenuBarVisibility>().dirty);

        // A repeated show command must not rewrite an already saved choice.
        let shown = "{ \"enabled\": true }\n";
        std::fs::write(&path, shown).unwrap();
        harness.world().write_message(Event::Command {
            command: Command::MenuBar(true),
        });
        harness.app.update();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shown);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn menu_updates_ignore_position_animation_but_track_menu_inputs() {
        use crate::ecs::layout::LayoutStrip;
        use crate::ecs::{ActiveWorkspaceMarker, Bounds, FocusedMarker, Position, Unmanaged};
        use bevy::app::{App, PostUpdate};
        use bevy::ecs::{resource::Resource, schedule::IntoScheduleConfigs, system::ResMut};
        use bevy::math::IVec2;

        #[derive(Resource, Default)]
        struct Updates(usize);
        fn count(mut updates: ResMut<Updates>) {
            updates.0 += 1;
        }
        let mut app = App::new();
        app.init_resource::<Updates>()
            .add_systems(PostUpdate, count.run_if(super::menu_bar_dirty));
        let strip = app
            .world_mut()
            .spawn((LayoutStrip::new(1, 0), ActiveWorkspaceMarker))
            .id();
        let window = app
            .world_mut()
            .spawn((
                Bounds(IVec2::new(400, 600)),
                Position(IVec2::ZERO),
                FocusedMarker,
            ))
            .id();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 1);
        for x in 1..=30 {
            app.world_mut().get_mut::<Position>(window).unwrap().0.x = x;
            app.update();
        }
        assert_eq!(app.world().resource::<Updates>().0, 1);
        app.world_mut().get_mut::<Bounds>(window).unwrap().0.x = 500;
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 2);
        app.world_mut()
            .entity_mut(window)
            .insert(Unmanaged::Floating);
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 3);
        app.world_mut().entity_mut(window).remove::<Unmanaged>();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 4);
        app.world_mut().entity_mut(window).remove::<FocusedMarker>();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 5);
        app.world_mut().get_mut::<Bounds>(window).unwrap().0.x = 600;
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 5);
        app.world_mut().entity_mut(window).insert(FocusedMarker);
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 6);
        app.world_mut()
            .get_mut::<LayoutStrip>(strip)
            .unwrap()
            .virtual_index = 1;
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 7);
        let offscreen = app.world_mut().spawn(LayoutStrip::new(1, 2)).id();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 8);
        app.world_mut().despawn(offscreen);
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 9);
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 9);
    }

    #[test]
    fn menu_commands_apply_in_the_same_tick_as_pumped_events() {
        use crate::tests::TestHarness;
        use bevy::app::PreUpdate;
        use bevy::ecs::{message::MessageWriter, schedule::IntoScheduleConfigs, system::Local};

        // Inject a command immediately before the real pump. The handler must
        // run after the pump and see it in this update, including its condition.
        fn incoming(mut events: MessageWriter<Event>, mut sent: Local<bool>) {
            if !*sent {
                events.write(Event::Command {
                    command: Command::MenuBar(false),
                });
                *sent = true;
            }
        }
        let mut harness = TestHarness::new();
        harness
            .app
            .add_systems(PreUpdate, incoming.before(crate::ecs::systems::pump_events));
        harness.app.update();
        let world = harness.world();
        assert!(
            !world
                .resource::<MenuBarVisibility>()
                .enabled(world.resource::<Config>())
        );
    }

    #[test]
    fn menu_width_state_tracks_display_and_dock_changes() {
        use crate::ecs::{ActiveDisplayMarker, DockPosition};
        use crate::manager::Display;
        use bevy::app::{App, PostUpdate};
        use bevy::ecs::{resource::Resource, schedule::IntoScheduleConfigs, system::ResMut};
        use bevy::math::IRect;

        #[derive(Resource, Default)]
        struct Updates(usize);
        fn count(mut updates: ResMut<Updates>) {
            updates.0 += 1;
        }
        let mut app = App::new();
        app.init_resource::<Updates>()
            .add_systems(PostUpdate, count.run_if(super::menu_viewport_dirty));
        let display = app
            .world_mut()
            .spawn((
                Display::new(1, IRect::new(0, 0, 1024, 768), 20),
                ActiveDisplayMarker,
            ))
            .id();
        app.update();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 1);
        app.world_mut()
            .entity_mut(display)
            .insert(DockPosition::Left(50));
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 2);
        app.world_mut().entity_mut(display).remove::<DockPosition>();
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 3);
        app.world_mut()
            .get_mut::<Display>(display)
            .unwrap()
            .set_menubar_height_override(Some(30));
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 4);
        app.update();
        assert_eq!(app.world().resource::<Updates>().0, 4);
    }

    #[test]
    fn width_selection_keeps_the_existing_tolerance() {
        let widths = [33, 34, 50, 67];
        assert_eq!(
            super::selected_width_percentages(&widths, Some(0.33333)),
            [33, 34]
        );
        assert_eq!(
            super::selected_width_percentages(&widths, Some(0.335)),
            [33, 34]
        );
        assert_eq!(super::selected_width_percentages(&widths, Some(0.5)), [50]);
        assert!(super::selected_width_percentages(&widths, None).is_empty());
    }

    #[test]
    fn virtual_workspace_label_is_one_based() {
        assert_eq!(virtual_workspace_label(0), "1");
        assert_eq!(virtual_workspace_label(4), "5");
    }

    #[test]
    fn paged_total_reads_as_the_last_workspace_label() {
        assert_eq!(
            indicator_label(
                IndicatorFormat::Default,
                paged_last_index(4),
                false,
                '☉',
                '○'
            ),
            "4"
        );
        assert_eq!(
            indicator_label(IndicatorFormat::Roman, paged_last_index(4), false, '☉', '○'),
            "IV"
        );
        // A lone workspace still reads as "1 / 1" rather than underflowing.
        assert_eq!(
            indicator_label(
                IndicatorFormat::Default,
                paged_last_index(1),
                false,
                '☉',
                '○'
            ),
            "1"
        );
        assert_eq!(
            indicator_label(
                IndicatorFormat::Default,
                paged_last_index(0),
                false,
                '☉',
                '○'
            ),
            "1"
        );
    }

    #[test]
    fn marked_labels_number_the_active_workspace_among_marks() {
        let marks: Vec<String> = [0, 1, 2, 3]
            .into_iter()
            .map(|index| indicator_label(IndicatorFormat::Marked, index, index == 1, '☉', '·'))
            .collect();
        assert_eq!(marks, ["·", "2", "·", "·"]);
    }

    #[test]
    fn unicode_labels_only_distinguish_active_from_inactive() {
        assert_eq!(
            indicator_label(IndicatorFormat::Unicode, 2, true, '☉', '○'),
            "☉"
        );
        assert_eq!(
            indicator_label(IndicatorFormat::Unicode, 2, false, '☉', '○'),
            "○"
        );
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
