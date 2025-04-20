use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFNumberGetValue, CFNumberType, CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{CGDirectDisplayID, CGEventFlags};
use std::io::{Error, ErrorKind, Result};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::thread::JoinHandle;
use stdext::function_name;

use crate::config::Config;
use crate::manager::WindowManager;
use crate::platform::{ProcessSerialNumber, WorkspaceObserver};
use crate::skylight::{
    ConnID, SLSCopyAssociatedWindows, SLSFindWindowAndOwner, SLSMainConnectionID, WinID,
};
use crate::util::{AxuWrapperType, get_array_values};
use crate::windows::{Window, WindowPane};

#[allow(dead_code)]
#[derive(Debug)]
pub enum Event {
    Exit,
    ProcessesLoaded,
    ConfigRefresh {
        config: Config,
    },

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
        window_id: WinID,
    },
    WindowFocused {
        window_id: WinID,
    },
    WindowMoved {
        window_id: WinID,
    },
    WindowResized {
        window_id: WinID,
    },
    WindowMinimized {
        window_id: WinID,
    },
    WindowDeminimized {
        window_id: WinID,
    },
    WindowTitleChanged {
        window_id: WinID,
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
    KeyDown {
        key: i64,
        modifier: CGEventFlags,
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
        window_id: WinID,
    },
    MenuClosed {
        window_id: WinID,
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

#[derive(Clone, Debug)]
pub struct EventSender {
    tx: Sender<Event>,
}

impl EventSender {
    fn new(tx: Sender<Event>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: Event) -> Result<()> {
        self.tx
            .send(event)
            .map_err(|err| {
                Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("{}: sending event: {err}", function_name!()),
                )
            })
            .inspect_err(|err| error!("{err}"))
    }
}

pub struct EventHandler {
    quit: Arc<AtomicBool>,
    tx: EventSender,
    rx: Receiver<Event>,
    main_cid: ConnID,
    window_manager: WindowManager,
    initial_scan: bool,
    mouse_down_window: Option<Window>,
    down_location: CGPoint,
}

impl EventHandler {
    pub fn new() -> Result<Self> {
        let main_cid = unsafe { SLSMainConnectionID() };
        info!("{}: My connection id: {main_cid}", function_name!());

        let (tx, rx) = channel::<Event>();
        let sender = EventSender::new(tx);
        Ok(EventHandler {
            quit: AtomicBool::new(false).into(),
            tx: sender.clone(),
            rx,
            main_cid,
            window_manager: WindowManager::new(sender, main_cid),
            initial_scan: true,
            mouse_down_window: None,
            down_location: CGPoint::default(),
        })
    }

    pub fn sender(&self) -> EventSender {
        self.tx.clone()
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
            let event = match self.rx.recv() {
                Err(_) => break,
                Ok(event) => event,
            };
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
                return self.window_manager.refresh_displays();
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
                if self.initial_scan {
                    self.window_manager.add_existing_process(&psn, observer);
                } else {
                    debug!("{}: ApplicationLaunched: {psn:?}", function_name!(),);
                    return self.window_manager.application_launched(&psn, observer);
                }
            }
            Event::WindowFocused { window_id } => self.window_focused(window_id),
            Event::WindowTitleChanged { window_id } => {
                trace!("{}: WindowTitleChanged: {window_id:?}", function_name!());
            }

            Event::Command { argv } => self.command(argv),

            Event::MenuClosed { window_id } => {
                trace!("{}: MenuClosed event: {window_id:?}", function_name!())
            }

            Event::KeyDown { key, modifier } => {
                self.key_pressed(key, modifier);
            }

            _ => self.window_manager.process_event(event)?,
        }
        Ok(())
    }

    fn key_pressed(&self, key: i64, eventflags: CGEventFlags) {
        // Normal key, left, right.
        const MASK_ALT: [u64; 3] = [0x00080000, 0x00000020, 0x00000040];
        const MASK_SHIFT: [u64; 3] = [0x00020000, 0x00000002, 0x00000004];
        const MASK_CMD: [u64; 3] = [0x00100000, 0x00000008, 0x00000010];
        const MASK_CTRL: [u64; 3] = [0x00040000, 0x00000001, 0x00002000];

        let modifier = if MASK_ALT.iter().any(|mask| *mask == (eventflags.0 & mask)) {
            "alt"
        } else if MASK_SHIFT.iter().any(|mask| *mask == (eventflags.0 & mask)) {
            "shift"
        } else if MASK_CMD.iter().any(|mask| *mask == (eventflags.0 & mask)) {
            "cmd"
        } else if MASK_CTRL.iter().any(|mask| *mask == (eventflags.0 & mask)) {
            "ctrl"
        } else {
            ""
        };

        info!("KeyDown: {modifier} {key}");
    }

    fn window_focused(&mut self, window_id: WinID) {
        debug!("{}: {}", function_name!(), window_id);
        if let Some(window) = self.window_manager.find_window(window_id) {
            if !window.app().is_frontmost() {
                return;
            }

            self.window_manager.window_focused(window);
        } else {
            warn!(
                "{}: window_manager_add_lost_focused_event",
                function_name!()
            );
            // TODO:
            // window_manager_add_lost_focused_event(&g_window_manager, window_id);
        }
    }

    fn get_window_in_direction(
        direction: &str,
        current_window_id: WinID,
        panel: &WindowPane,
    ) -> Option<WinID> {
        let mut found: Option<WinID> = None;
        let accessor = |window_id: WinID| {
            if window_id == current_window_id {
                // If it's the same window, continue
                true
            } else {
                found = Some(window_id);
                false
            }
        };
        _ = match direction {
            "west" => panel.access_left_of(current_window_id, accessor),
            "east" => panel.access_right_of(current_window_id, accessor),
            "first" => {
                found = panel.first().ok();
                Ok(())
            }
            "last" => {
                found = panel.last().ok();
                Ok(())
            }
            dir => {
                error!("{}: Unhandled direction {dir}", function_name!());
                Ok(())
            }
        }
        .inspect_err(|err| debug!("{}: panel operation: {err}", function_name!()));

        found
    }

    fn command_move_focus(
        &self,
        argv: &[String],
        current_window: &Window,
        panel: &WindowPane,
    ) -> Option<WinID> {
        let direction = argv.first()?;

        EventHandler::get_window_in_direction(direction, current_window.id(), panel).inspect(
            |window_id| {
                if let Some(window) = self.window_manager.find_window(*window_id) {
                    window.focus_with_raise();
                }
            },
        )
    }

    fn command_swap_focus(
        &self,
        argv: &[String],
        current_window: &Window,
        panel: &WindowPane,
        bounds: &CGRect,
    ) -> Option<Window> {
        let direction = argv.first()?;
        let index = panel.index_of(current_window.id()).ok()?;
        let window = EventHandler::get_window_in_direction(direction, current_window.id(), panel)
            .and_then(|window_id| self.window_manager.find_window(window_id))?;
        let new_index = panel.index_of(window.id()).ok()?;

        let origin = if new_index == 0 {
            // If reached far left, snap the window to left.
            CGPoint::new(0.0, 0.0)
        } else if new_index == (panel.len() - 1) {
            // If reached full right, snap the window to right.
            CGPoint::new(bounds.size.width - current_window.frame().size.width, 0.0)
        } else {
            panel
                .get(new_index)
                .ok()
                .and_then(|window_id| self.window_manager.find_window(window_id))?
                .frame()
                .origin
        };
        current_window.reposition(origin.x, origin.y);
        if index < new_index {
            (index..new_index).for_each(|idx| panel.swap(idx, idx + 1));
        } else {
            (new_index..index)
                .rev()
                .for_each(|idx| panel.swap(idx, idx + 1));
        }
        Some(window)
    }

    fn command_windows(&mut self, argv: &[String]) -> Result<()> {
        let empty = "".to_string();
        let window = match self
            .window_manager
            .focused_window
            .and_then(|window_id| self.window_manager.find_window(window_id))
            .filter(|window| window.is_eligible())
        {
            Some(window) => window,
            None => {
                warn!("{}: No window focused.", function_name!());
                return Ok(());
            }
        };

        // If unable to detect current display, a new one must have been added, so refresh the
        // current displays and reshuffle the windows.
        let active_display = match self.window_manager.active_display() {
            Ok(display) => display,
            _ => {
                self.window_manager.refresh_displays()?;
                self.window_manager.active_display()?
            }
        };
        let active_panel = active_display.active_panel(self.main_cid)?;
        let display_bounds = self.window_manager.current_display_bounds()?;

        match argv.first().unwrap_or(&empty).as_ref() {
            "focus" => {
                self.command_move_focus(&argv[1..], &window, &active_panel);
            }

            "swap" => {
                self.command_swap_focus(&argv[1..], &window, &active_panel, &display_bounds);
            }

            "center" => {
                let frame = window.frame();
                window.reposition(
                    (display_bounds.size.width - frame.size.width) / 2.0,
                    frame.origin.y,
                );
                window.center_mouse(self.main_cid);
            }

            "resize" => {
                let width_ratio = window.next_size_ratio();
                // let frame = window.inner().frame;
                // window.reposition((SCREEN_WIDTH - width) / 2.0, frame.origin.y);
                window.resize(
                    width_ratio * display_bounds.size.width,
                    window.frame().size.height,
                    &display_bounds,
                );
            }

            "manage" => {
                if window.managed() {
                    // Window already managed, remove it from the managed stack.
                    active_panel.remove(window.id());
                    window.manage(false);
                } else {
                    // Add newly managed window to the stack.
                    let frame = window.frame();
                    window.reposition(frame.origin.x, 0.0);
                    window.resize(
                        frame.size.width,
                        display_bounds.size.height,
                        &display_bounds,
                    );
                    active_panel.append(window.id());
                    window.manage(true);
                };
            }

            _ => (),
        };
        self.window_manager.reshuffle_around(&window)
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
        if self.window_manager.mission_control_is_active() {
            return Ok(());
        }

        let window = self.find_window_at_point(point)?;
        if !window.fully_visible(&self.window_manager.current_display_bounds()?) {
            self.window_manager.reshuffle_around(&window)?;
        }

        self.mouse_down_window = Some(window);
        self.down_location = *point;

        Ok(())
    }

    fn mouse_up(&mut self, point: &CGPoint) {
        info!("{}: {point:?}", function_name!());
    }

    fn mouse_dragged(&self, point: &CGPoint) {
        info!("{}: {point:?}", function_name!());

        if self.window_manager.mission_control_is_active() {
            #[warn(clippy::needless_return)]
            return;
        }
    }

    fn mouse_moved(&mut self, point: &CGPoint) -> Result<()> {
        if !self.window_manager.focus_follows_mouse() {
            return Ok(());
        }
        if self.window_manager.mission_control_is_active() {
            return Ok(());
        }
        if self.window_manager.ffm_window_id.is_some() {
            trace!("{}: ffm_window_id > 0", function_name!());
            return Ok(());
        }

        match self.find_window_at_point(point) {
            Ok(window) => {
                let window_id = window.id();
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
                            NonNull::from(&mut child_wid).as_ptr().cast(),
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
                    let child_window = match self.window_manager.find_window(child_wid) {
                        None => {
                            warn!(
                                "{}: Unable to find child window {child_wid}.",
                                function_name!()
                            );
                            continue;
                        }
                        Some(window) => window,
                    };

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
                        window = child_window.clone();
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
