use bevy::ecs::entity::Entity;
use bevy::ecs::query::With;
use bevy::ecs::system::{Local, NonSendMut, Query};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSStatusBar, NSStatusItem, NSVariableStatusItemLength};
use objc2_core_foundation::CGFloat;
use objc2_foundation::NSString;
use tracing::warn;

use crate::ecs::ActiveWorkspaceMarker;
use crate::ecs::layout::LayoutStrip;

pub struct MenuBarManager {
    mtm: MainThreadMarker,
    status_bar: Retained<NSStatusBar>,
    status_item: Retained<NSStatusItem>,
    current_label: Option<String>,
}

const STATUS_ITEM_BACKGROUND_ALPHA: CGFloat = 0.18;
const STATUS_ITEM_CORNER_RADIUS: CGFloat = 5.0;

impl MenuBarManager {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        status_item.setVisible(true);

        let mut manager = Self {
            mtm,
            status_bar,
            status_item,
            current_label: None,
        };
        manager.show_virtual_workspace(0);
        manager
    }

    pub fn show_virtual_workspace(&mut self, virtual_index: u32) {
        let label = format_virtual_workspace_label(virtual_index);
        if self.current_label.as_deref() == Some(label.as_str()) {
            return;
        }

        let title = NSString::from_str(&label);
        let tooltip = NSString::from_str("Paneru virtual workspace");
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

pub fn update_virtual_workspace_status_item(
    active_workspace: Query<(Entity, &LayoutStrip), With<ActiveWorkspaceMarker>>,
    menu_bar: Option<NonSendMut<MenuBarManager>>,
    mut displayed_workspace: Local<Option<(Entity, u32)>>,
) {
    let Some(mut menu_bar) = menu_bar else {
        return;
    };
    let Some((entity, strip)) = active_workspace.iter().next() else {
        return;
    };

    let next = (entity, strip.virtual_index);
    if displayed_workspace
        .as_ref()
        .is_some_and(|displayed| *displayed == next)
    {
        return;
    }

    menu_bar.show_virtual_workspace(strip.virtual_index);
    *displayed_workspace = Some(next);
}

pub(crate) fn format_virtual_workspace_label(virtual_index: u32) -> String {
    format!("VW {}", virtual_index + 1)
}

#[cfg(test)]
mod tests {
    use super::format_virtual_workspace_label;

    #[test]
    fn label_is_one_based() {
        assert_eq!(format_virtual_workspace_label(0), "VW 1");
        assert_eq!(format_virtual_workspace_label(4), "VW 5");
    }
}
