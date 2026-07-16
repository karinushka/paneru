use bevy::ecs::entity::Entity;
use bevy::ecs::query::{Has, With};
use bevy::ecs::system::{NonSendMut, Query, Res};
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSColor, NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{NSObject, NSString};
use tracing::warn;

use crate::accessibility_prompt::{AccessibilitySetupAction, show_accessibility_setup};
use crate::commands::{Command, Operation};
use crate::config::Config;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker, Unmanaged,
};
use crate::events::{Event, EventSender};
use crate::manager::{Display, request_ax_privilege};

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

pub struct MenuBarManager {
    mtm: MainThreadMarker,
    status_bar: Retained<NSStatusBar>,
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    action_target: Retained<MenuActionTarget>,
    width_items: Vec<(i32, Retained<NSMenuItem>)>,
    managed_window_items: Vec<Retained<NSMenuItem>>,
    manage_item: Option<Retained<NSMenuItem>>,
    configured_widths: Vec<i32>,
    current_label: Option<String>,
}

const STATUS_ITEM_BACKGROUND_ALPHA: CGFloat = 0.18;
const STATUS_ITEM_CORNER_RADIUS: CGFloat = 5.0;

#[derive(Debug, Eq, PartialEq)]
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
            configured_widths: Vec::new(),
            current_label: None,
        }
    }

    pub fn new_accessibility_required(mtm: MainThreadMarker, events: EventSender) -> Self {
        let mut manager = Self::new(mtm, events);
        manager.rebuild_accessibility_menu();
        manager.show_label("Paneru !".to_owned());
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
        show_virtual_workspace: bool,
        preset_widths: &[f64],
        has_focused_window: bool,
        focused_width_ratio: Option<f64>,
    ) {
        let widths = normalized_width_percentages(preset_widths);
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
        for (percentage, item) in &self.width_items {
            let selected = focused_width_ratio
                .is_some_and(|ratio| (ratio.mul_add(100.0, -f64::from(*percentage))).abs() < 1.0);
            item.setState(if selected {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }

        let label = if show_virtual_workspace {
            format_virtual_workspace_label(virtual_index)
        } else {
            "Paneru".to_owned()
        };
        self.show_label(label);
    }

    fn rebuild_menu(&mut self, widths: &[i32]) {
        self.menu.removeAllItems();
        self.width_items.clear();
        self.managed_window_items.clear();
        self.manage_item = None;

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

    fn show_label(&mut self, label: String) {
        if self.current_label.as_deref() == Some(label.as_str()) {
            return;
        }

        let title = NSString::from_str(&label);
        let tooltip = NSString::from_str("Paneru window manager");
        let Some(button) = self.status_item.button(self.mtm) else {
            warn!("unable to update menu bar: status item has no button");
            return;
        };

        button.setWantsLayer(true);
        if let Some(layer) = button.layer() {
            let background = NSColor::controlAccentColor()
                .colorWithAlphaComponent(STATUS_ITEM_BACKGROUND_ALPHA)
                .CGColor();
            layer.setBackgroundColor(Some(&background));
            layer.setCornerRadius(STATUS_ITEM_CORNER_RADIUS);
            layer.setMasksToBounds(true);
        }
        button.setTitle(&title);
        button.setToolTip(Some(&tooltip));
        self.current_label = Some(label);
    }
}

impl Drop for MenuBarManager {
    fn drop(&mut self) {
        self.status_bar.removeStatusItem(&self.status_item);
    }
}

#[allow(clippy::needless_pass_by_value, clippy::type_complexity)]
pub fn update_menu_bar(
    active_workspace: Query<(Entity, &LayoutStrip), With<ActiveWorkspaceMarker>>,
    active_display: Query<(&Display, Option<&DockPosition>), With<ActiveDisplayMarker>>,
    focused: Query<(&Bounds, Has<Unmanaged>), With<FocusedMarker>>,
    config: Res<Config>,
    menu_bar: Option<NonSendMut<MenuBarManager>>,
) {
    let Some(mut menu_bar) = menu_bar else {
        return;
    };
    let Some((_, strip)) = active_workspace.iter().next() else {
        return;
    };

    let focused_window = focused.iter().next();
    let focused_width_ratio = active_display.iter().next().zip(focused_window).and_then(
        |((display, dock), (bounds, unmanaged))| {
            (!unmanaged).then(|| {
                f64::from(bounds.0.x)
                    / f64::from(display.actual_display_bounds(dock, &config).width())
            })
        },
    );

    menu_bar.update(
        strip.virtual_index,
        config.workspace_menu_status(),
        &config.preset_column_widths(),
        focused_window.is_some(),
        focused_width_ratio,
    );
}

pub(crate) fn format_virtual_workspace_label(virtual_index: u32) -> String {
    format!("VW {}", virtual_index + 1)
}

fn normalized_width_percentages(widths: &[f64]) -> Vec<i32> {
    let mut percentages = widths
        .iter()
        .copied()
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
        .map(|ratio| ratio.mul_add(100.0, 0.0).round() as i32)
        .filter(|percentage| *percentage > 0)
        .collect::<Vec<_>>();
    percentages.sort_unstable();
    percentages.dedup();
    percentages
}

#[cfg(test)]
mod tests {
    use super::{
        WindowMenuEnablement, format_virtual_workspace_label, normalized_width_percentages,
        window_menu_enablement,
    };

    #[test]
    fn label_is_one_based() {
        assert_eq!(format_virtual_workspace_label(0), "VW 1");
        assert_eq!(format_virtual_workspace_label(4), "VW 5");
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
