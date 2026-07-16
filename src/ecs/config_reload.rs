use std::pin::Pin;

use bevy::ecs::message::MessageReader;
use bevy::ecs::system::{NonSend, NonSendMut, Query, Res, ResMut};
use notify::event::{DataChange, MetadataKind, ModifyKind};
use notify::{EventKind, Watcher};
use tracing::{debug, error, info};

use super::params::Windows;
use crate::config::Config;
use crate::errors::Result;
use crate::events::Event;
use crate::manager::{Application, Display, WindowManager};
use crate::platform::{AxMainThread, PlatformCallbacks};
use crate::util::symlink_target;

fn replace_config_transactionally(
    current: &mut Config,
    candidate: Config,
    reconfigure: impl FnOnce(&Config) -> Result<()>,
) -> Result<()> {
    reconfigure(&candidate)?;
    *current = candidate;
    Ok(())
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(super) fn refresh_configuration_trigger(
    _main_thread: NonSend<AxMainThread>,
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mut config: ResMut<Config>,
    mut watcher: Option<NonSendMut<Box<dyn Watcher>>>,
    windows: Windows,
    mut displays: Query<&mut Display>,
    applications: Query<&Application>,
    mut platform: Option<NonSendMut<Pin<Box<PlatformCallbacks>>>>,
) {
    for event in messages.read() {
        let Event::ConfigRefresh(event) = event else {
            continue;
        };
        let Some(ref mut watcher) = watcher else {
            continue;
        };
        match &event.kind {
            EventKind::Modify(
                ModifyKind::Metadata(MetadataKind::WriteTime)
                | ModifyKind::Data(DataChange::Content),
            ) => (),
            EventKind::Remove(_) => {
                for path in &event.paths {
                    _ = watcher.unwatch(path).inspect_err(|err| {
                        error!("unwatching the config '{}': {err}", path.display());
                    });
                }
                continue;
            }
            _ => continue,
        }

        let mut candidate = config.clone();
        let mut reloaded = false;
        for path in &event.paths {
            if let Some(symlink) = symlink_target(path) {
                debug!(
                    "symlink '{}' changed, replacing the watcher.",
                    symlink.display()
                );
                if let Ok(new_watcher) =
                    window_manager
                        .setup_config_watcher(path)
                        .inspect_err(|err| {
                            error!("watching the config '{}': {err}", path.display());
                        })
                {
                    **watcher = new_watcher;
                }
            }
            info!("Reloading configuration file; {}", path.display());
            reloaded |= candidate
                .reload_config(path.as_path())
                .inspect_err(|err| {
                    error!("loading config '{}': {err}", path.display());
                })
                .is_ok();
        }

        if reloaded
            && let Err(err) = replace_config_transactionally(&mut config, candidate, |candidate| {
                platform.as_mut().map_or(Ok(()), |platform| {
                    platform.reconfigure_input_handler(candidate).map(|_| ())
                })
            })
        {
            error!("reconfiguring input handler; keeping previous config: {err}");
            continue;
        }

        let height = config.menubar_height();
        for mut display in &mut displays {
            display.set_menubar_height_override(height);
        }
        if let Some((window, _, parent)) = windows
            .focused()
            .and_then(|(w, e)| windows.find_parent(w.id()).map(|(w, _, p)| (w, e, p)))
            && let Ok(app) = applications.get(parent)
        {
            super::triggers::update_passthrough(window, app, &config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::Error;

    #[test]
    fn failed_input_tap_replacement_keeps_running_config() {
        let mut current = Config::default();
        let running_focus_follows_mouse = current.focus_follows_mouse();
        let candidate = Config::try_from("[options]\nfocus_follows_mouse = true\n\n[bindings]\n")
            .expect("candidate config should parse");

        let result = replace_config_transactionally(&mut current, candidate, |_| {
            Err(Error::InvalidInput("injected tap creation failure".into()))
        });

        assert!(result.is_err());
        assert_eq!(current.focus_follows_mouse(), running_focus_follows_mouse);
    }
}
