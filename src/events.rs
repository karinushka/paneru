use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFNumberGetValue, CFNumberType, CFRetained, CGPoint, CGRect};
use objc2_core_graphics::CGDirectDisplayID;
use std::ffi::c_void;
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use stdext::function_name;
use stdext::prelude::RwLockExt;

use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::process::ProcessManager;
use crate::skylight::{
    ConnID, SLSCopyAssociatedWindows, SLSFindWindowAndOwner, SLSMainConnectionID, WinID,
};
use crate::util::{get_array_values, AxuWrapperType};
use crate::windows::{Window, WindowManager};

#[allow(dead_code)]
#[derive(Debug)]
pub enum Event {
    Exit,
    ProcessesLoaded,

    ApplicationLaunched {
        psn: ProcessSerialNumber,
        observer: Retained<WorkspaceObserver>,
    },

    ApplicationTerminated {
        psn: ProcessSerialNumber,
    },
    ApplicationFrontSwitched {
        psn: ProcessSerialNumber,
    },
    ApplicationActivated {
        msg: String,
    },
    ApplicationDeactivated,
    ApplicationVisible {
        msg: String,
    },
    ApplicationHidden {
        msg: String,
    },

    WindowCreated {
        element: CFRetained<AxuWrapperType>,
    },
    WindowDestroyed {
        window_id: Option<WinID>,
    },
    WindowFocused {
        window_id: Option<WinID>,
    },
    WindowMoved {
        window_id: Option<WinID>,
    },
    WindowResized {
        window_id: Option<WinID>,
    },
    WindowMinimized {
        window_id: Option<WinID>,
    },
    WindowDeminimized {
        window_id: Option<WinID>,
    },
    WindowTitleChanged {
        window_id: Option<WinID>,
    },

    MouseDown {
        point: CGPoint,
    },
    MouseUp {
        point: CGPoint,
    },
    MouseDragged {
        point: CGPoint,
    },
    MouseMoved {
        point: CGPoint,
    },

    SpaceCreated,
    SpaceDestroyed,
    SpaceChanged {
        msg: String,
    },

    DisplayAdded {
        display_id: CGDirectDisplayID,
    },
    DisplayRemoved {
        display_id: CGDirectDisplayID,
    },
    DisplayMoved {
        display_id: CGDirectDisplayID,
    },
    DisplayResized {
        display_id: CGDirectDisplayID,
    },
    DisplayChanged {
        msg: String,
    },

    MissionControlShowAllWindows,
    MissionControlShowFrontWindows,
    MissionControlShowDesktop,
    MissionControlExit,

    DockDidChangePref {
        msg: String,
    },
    DockDidRestart {
        msg: String,
    },

    MenuOpened {
        window_id: Option<WinID>,
    },
    MenuClosed {
        window_id: Option<WinID>,
    },
    MenuBarHiddenChanged {
        msg: String,
    },
    SystemWoke {
        msg: String,
    },

    Command {
        argv: Vec<String>,
    },

    TypeCount,
}

pub struct EventHandler {
    quit: Arc<AtomicBool>,
    rx: Receiver<Event>,
    main_cid: ConnID,
    process_manager: ProcessManager,
    window_manager: WindowManager,
    initial_scan: bool,
}

impl EventHandler {
    pub fn new(tx: Sender<Event>, rx: Receiver<Event>) -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        info!("{}: My connection id: {main_cid}", function_name!());

        EventHandler {
            quit: AtomicBool::new(false).into(),
            rx,
            main_cid,
            process_manager: ProcessManager::default(),
            window_manager: WindowManager::new(tx, main_cid),
            initial_scan: true,
        }
    }

    pub fn start(mut self) -> (Arc<AtomicBool>, JoinHandle<()>) {
        let quit = self.quit.clone();
        let handle = thread::spawn(move || {
            self.run();
        });
        (quit, handle)
    }

    fn run(&mut self) {
        loop {
            let e = self.rx.recv();
            if e.is_err() {
                break;
            }
            let event = e.unwrap();
            trace!("{}: Event {event:?}", function_name!());

            if matches!(event, Event::Exit) {
                break;
            }

            _ = self
                .process_event(event)
                .inspect_err(|err| match err.kind() {
                    // TODO: for now we'll treat the Other return values as non-error ones.
                    // This can be adjusted later with a custom error space.
                    ErrorKind::Other => trace!("{err}"),
                    kind => error!("ApplicationLaunched: {kind} {err}"),
                });
        }
    }

    fn process_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Exit => return Ok(()),
            Event::ProcessesLoaded => {
                info!(
                    "{}: === Processes loaded - loading windows ===",
                    function_name!()
                );
                self.initial_scan = false;
                self.window_manager.start(&mut self.process_manager);
            }
            Event::MouseDown { point } => {
                return self.mouse_down(&point);
            }

            Event::MouseUp { point } => self.mouse_up(&point),
            Event::MouseMoved { point } => return self.mouse_moved(&point),
            Event::MouseDragged { point } => self.mouse_dragged(&point),

            // TODO: remove this handler. Used to test delivery of events.
            Event::ApplicationActivated { msg } => {
                trace!("Application activated: {msg}");
            }

            Event::ApplicationLaunched { psn, observer } => {
                let process = self.process_manager.process_add(&psn, observer)?;
                if !self.initial_scan {
                    debug!(
                        "{}: ApplicationLaunched: {}",
                        function_name!(),
                        process.name
                    );
                    return process.application_launched(&mut self.window_manager);
                }
            }
            Event::ApplicationTerminated { psn } => {
                self.process_manager.process_delete(&psn);
            }
            Event::ApplicationFrontSwitched { psn } => {
                let process = self.process_manager.find_process(&psn)?;
                self.window_manager.front_switched(process);
            }

            Event::WindowCreated { element } => {
                return self.window_manager.window_created(element);
            }
            Event::WindowDestroyed { window_id } => {
                if let Some(window_id) = window_id {
                    self.window_manager.window_destroyed(window_id)
                }
            }
            Event::WindowFocused { window_id } => {
                if let Some(window_id) = window_id {
                    self.window_focused(window_id)
                }
            }
            Event::WindowMoved { window_id } => {
                if let Some(window_id) = window_id {
                    self.window_manager.window_moved(window_id)
                }
            }
            Event::WindowResized { window_id } => {
                if let Some(window_id) = window_id {
                    return self.window_manager.window_resized(window_id);
                }
            }
            Event::WindowTitleChanged { window_id } => {
                trace!("{}: WindowTitleChanged: {window_id:?}", function_name!());
            }

            Event::MissionControlShowAllWindows
            | Event::MissionControlShowFrontWindows
            | Event::MissionControlShowDesktop => {
                self.window_manager.mission_control_is_active = true;
            }
            Event::MissionControlExit => {
                self.window_manager.mission_control_is_active = false;
            }

            Event::Command { argv } => self.command(argv),

            Event::MenuClosed { window_id } => {
                trace!("{}: MenuClosed event: {window_id:?}", function_name!())
            }

            _ => info!("{}: Unhandled event {event:?}", function_name!()),
        }
        Ok(())
    }

    fn window_focused(&mut self, window_id: WinID) {
        debug!("{}: {}", function_name!(), window_id);
        if let Some(window) = self.window_manager.find_window(window_id) {
            if !window.inner().app.is_frontmost() {
                return;
            }

            window.did_receive_focus(&mut self.window_manager);
        } else {
            warn!(
                "{}: window_manager_add_lost_focused_event",
                function_name!()
            );
            // TODO:
            // window_manager_add_lost_focused_event(&g_window_manager, window_id);
        }
    }

    fn get_focused_index(focus: Option<&Window>, panel: &[Window]) -> Option<usize> {
        focus.and_then(|window| {
            let focused_id = window.inner().id;
            panel
                .iter()
                .position(|window| window.inner().id == focused_id)
        })
    }

    fn get_panel_in_direction(
        direction: &str,
        focus: Option<&Window>,
        panel: &[Window],
    ) -> Option<(usize, usize)> {
        let index = EventHandler::get_focused_index(focus, panel)?;
        let new_index = match direction {
            "west" => {
                if index > 0 {
                    index - 1
                } else {
                    index
                }
            }
            "east" => {
                if index >= panel.len() - 1 {
                    panel.len() - 1
                } else {
                    index + 1
                }
            }
            "first" => 0,
            "last" => panel.len() - 1,
            _ => index,
        };

        (index != new_index).then_some((index, new_index))
    }

    fn command_move_focus(
        argv: &[String],
        focus: Option<&Window>,
        panel: &[Window],
    ) -> Option<usize> {
        let empty = "".to_string();
        let direction = argv.first().unwrap_or(&empty);

        EventHandler::get_panel_in_direction(direction, focus, panel).map(|(_, new_index)| {
            let window = &panel[new_index];
            window.focus_with_raise();
            new_index
        })
    }

    fn command_swap_focus(
        argv: &[String],
        focus: Option<&Window>,
        panel: &mut [Window],
        bounds: &CGRect,
    ) -> Option<usize> {
        let empty = "".to_string();
        let direction = argv.first().unwrap_or(&empty);

        EventHandler::get_panel_in_direction(direction, focus, panel).map(|(index, new_index)| {
            let origin = if new_index == 0 {
                // If reached far left, snap the window to left.
                CGPoint::new(0.0, 0.0)
            } else if new_index == (panel.len() - 1) {
                // If reached full right, snap the window to right.
                CGPoint::new(
                    bounds.size.width - panel[index].inner().frame.size.width,
                    0.0,
                )
            } else {
                panel[new_index].inner().frame.origin
            };
            panel[index].reposition(origin.x, origin.y);
            if index < new_index {
                (index..new_index).for_each(|idx| panel.swap(idx, idx + 1));
            } else {
                (new_index..index)
                    .rev()
                    .for_each(|idx| panel.swap(idx, idx + 1));
            }
            new_index
        })
    }

    fn command_windows(&mut self, argv: &[String]) -> Result<()> {
        let empty = "".to_string();
        let focus = self
            .window_manager
            .focused_window
            .and_then(|window_id| self.window_manager.find_window(window_id))
            .filter(|window| window.is_eligible());

        let active_panel = self.window_manager.active_panel()?;

        let display_bounds = self.window_manager.active_display()?.bounds;

        let window = match argv.first().unwrap_or(&empty).as_ref() {
            "focus" => {
                let index = EventHandler::command_move_focus(
                    &argv[1..],
                    focus.as_ref(),
                    active_panel.force_write().as_slice(),
                );
                index.and_then(|index| active_panel.force_read().get(index).cloned())
            }

            "swap" => {
                let mut panel = active_panel.force_write();
                EventHandler::command_swap_focus(
                    &argv[1..],
                    focus.as_ref(),
                    panel.as_mut_slice(),
                    &display_bounds,
                )
                .map(|new_index| panel[new_index].clone())
            }

            "center" => focus.inspect(|window| {
                let frame = window.inner().frame;
                window.reposition(
                    (display_bounds.size.width - frame.size.width) / 2.0,
                    frame.origin.y,
                );
                window.center_mouse(self.main_cid);
            }),

            "resize" => focus.inspect(|window| {
                window.inner.force_write().size_ratios.rotate_left(1);
                let width_ratio = *window.inner().size_ratios.first().unwrap();
                // let frame = window.inner().frame;
                // window.reposition((SCREEN_WIDTH - width) / 2.0, frame.origin.y);
                let frame = window.inner().frame;
                window.resize(width_ratio * display_bounds.size.width, frame.size.height);
            }),

            "manage" => focus.inspect(|window| {
                let window_id = window.inner().id;
                let index = active_panel
                    .force_read()
                    .iter()
                    .position(|item| item.inner().id == window_id);

                if let Some(index) = index {
                    // Window already managed, remove it from the managed stack.
                    active_panel.force_write().remove(index);
                } else {
                    // Add newly managed window to the stack.
                    let frame = window.inner().frame;
                    window.reposition(frame.origin.x, 0.0);
                    window.resize(frame.size.width, display_bounds.size.height);
                    active_panel.force_write().push(window.clone());
                }
            }),

            _ => None,
        };
        if let Some(window) = window {
            self.window_manager.reshuffle_around(&window)
        }
        Ok(())
    }

    fn command(&mut self, argv: Vec<String>) {
        if let Some(first) = argv.first() {
            match first.as_ref() {
                "window" => {
                    _ = self
                        .command_windows(&argv[1..])
                        .inspect_err(|err| warn!("{}: {err}", function_name!()))
                }
                "quit" => self.quit.store(true, std::sync::atomic::Ordering::Relaxed),
                _ => warn!("{}: Unhandled command: {argv:?}", function_name!()),
            }
        };
    }

    fn mouse_down(&mut self, point: &CGPoint) -> Result<()> {
        info!("{}: {point:?}", function_name!());
        if self.window_manager.mission_control_is_active {
            return Ok(());
        }

        let window = self.find_window_at_point(point)?;
        if !window.fully_visible(&self.window_manager) {
            self.window_manager.reshuffle_around(&window);
        }

        self.window_manager.mouse_down_window = Some(window);
        self.window_manager.down_location = *point;

        Ok(())
    }

    fn mouse_up(&mut self, point: &CGPoint) {
        info!("{}: {point:?}", function_name!());
    }

    fn mouse_dragged(&self, point: &CGPoint) {
        info!("{}: {point:?}", function_name!());

        if self.window_manager.mission_control_is_active {
            #[warn(clippy::needless_return)]
            return;
        }
    }

    fn mouse_moved(&mut self, point: &CGPoint) -> Result<()> {
        if self.window_manager.mission_control_is_active {
            return Ok(());
        }
        if self.window_manager.ffm_window_id.is_some() {
            trace!("{}: ffm_window_id > 0", function_name!());
            return Ok(());
        }

        match self.find_window_at_point(point) {
            Ok(window) => {
                let window_id = window.inner().id;
                if self
                    .window_manager
                    .focused_window
                    .is_some_and(|id| id == window_id)
                {
                    trace!("{}: allready focused {}", function_name!(), window_id);
                    return Ok(());
                }
                if !window.is_eligible() {
                    trace!("{}: {} not eligible", function_name!(), window_id);
                    return Ok(());
                }

                let window_list = unsafe {
                    let arr_ref = SLSCopyAssociatedWindows(self.main_cid, window_id);
                    CFRetained::retain(arr_ref)
                };

                let mut window = window;
                for item in get_array_values(window_list.deref()) {
                    let mut child_wid: WinID = 0;
                    unsafe {
                        if !CFNumberGetValue(
                            item.as_ref(),
                            CFNumberType::SInt32Type,
                            (&mut child_wid as *mut WinID) as *mut c_void,
                        ) {
                            warn!(
                                "{}: Unable to find subwindows of window {}: {item:?}.",
                                function_name!(),
                                window_id
                            );
                            continue;
                        }
                    };
                    debug!(
                        "{}: checking {}'s childen: {}",
                        function_name!(),
                        window_id,
                        child_wid
                    );
                    let child_window = self.window_manager.find_window(child_wid);
                    if child_window.is_none() {
                        warn!(
                            "{}: Unable to find child window {child_wid}.",
                            function_name!()
                        );
                        continue;
                    }

                    let role = match window.role() {
                        Ok(role) => role,
                        Err(err) => {
                            warn!("{}: finding role for {window_id}: {err}", function_name!(),);
                            continue;
                        }
                    };

                    // bool valid = CFEqual(role, kAXSheetRole) || CFEqual(role, kAXDrawerRole);
                    let valid = ["AXSheet", "AXDrawer"]
                        .iter()
                        .any(|axrole| axrole.eq(&role));

                    if valid {
                        window = child_window.unwrap().clone();
                        break;
                    }
                }

                //  Do not reshuffle windows due to moved mouse focus.
                self.window_manager.skip_reshuffle = true;

                window.focus_without_raise(&self.window_manager);
                self.window_manager.ffm_window_id = Some(window_id);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn find_window_at_point(&self, point: &CGPoint) -> Result<Window> {
        let mut window_id: WinID = 0;
        let mut window_cid: ConnID = 0;
        let mut window_point = CGPoint { x: 0f64, y: 0f64 };
        unsafe {
            SLSFindWindowAndOwner(
                self.main_cid,
                0, // filter window id
                1,
                0,
                point,
                &mut window_point,
                &mut window_id,
                &mut window_cid,
            )
        };
        if self.main_cid == window_cid {
            unsafe {
                SLSFindWindowAndOwner(
                    self.main_cid,
                    window_id,
                    -1,
                    0,
                    point,
                    &mut window_point,
                    &mut window_id,
                    &mut window_cid,
                )
            };
        }
        self.window_manager.find_window(window_id).ok_or(Error::new(
            ErrorKind::Other,
            format!("{}: could not find a window at {point:?}", function_name!()),
        ))
    }
}
