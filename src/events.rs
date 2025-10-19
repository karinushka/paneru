use log::{debug, error, info, trace, warn};
use objc2::rc::Retained;
use objc2_core_foundation::{CFNumber, CFNumberType, CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{CGDirectDisplayID, CGEventFlags};
use std::io::{Error, ErrorKind, Result};
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
use crate::windows::{Panel, Window, WindowPane};

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

    Swipe {
        delta_x: f64,
    },

    SpaceCreated,
    SpaceDestroyed,
    SpaceChanged,

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
    DisplayConfigured {
        display_id: CGDirectDisplayID,
    },
    DisplayChanged,

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
    /// Creates a new `EventSender` instance.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sending half of an MPSC channel.
    ///
    /// # Returns
    ///
    /// A new `EventSender`.
    fn new(tx: Sender<Event>) -> Self {
        Self { tx }
    }

    /// Sends an `Event` through the internal MPSC channel.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to send.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is sent successfully, otherwise `Err(Error)`.
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
    /// Creates a new `EventHandler` instance. It initializes the main connection ID and `WindowManager`.
    ///
    /// # Returns
    ///
    /// `Ok(Self)` if the `EventHandler` is created successfully, otherwise `Err(Error)`.
    pub fn new() -> Self {
        let main_cid = unsafe { SLSMainConnectionID() };
        debug!("{}: My connection id: {main_cid}", function_name!());

        let (tx, rx) = channel::<Event>();
        let sender = EventSender::new(tx);
        EventHandler {
            quit: AtomicBool::new(false).into(),
            tx: sender.clone(),
            rx,
            main_cid,
            window_manager: WindowManager::new(sender, main_cid),
            initial_scan: true,
            mouse_down_window: None,
            down_location: CGPoint::default(),
        }
    }

    /// Returns a clone of the `EventSender` for sending events to this handler.
    ///
    /// # Returns
    ///
    /// A cloned `EventSender`.
    pub fn sender(&self) -> EventSender {
        self.tx.clone()
    }

    /// Starts the event handler in a new thread.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// * An `Arc<AtomicBool>` used to signal the handler to quit.
    /// * A `JoinHandle` for the spawned thread.
    pub fn start(mut self) -> (Arc<AtomicBool>, JoinHandle<()>) {
        let quit = self.quit.clone();
        let handle = thread::spawn(move || {
            self.run();
        });
        (quit, handle)
    }

    /// The main run loop for the event handler. It continuously receives and processes events until an `Exit` event or channel disconnection.
    fn run(&mut self) {
        loop {
            let Ok(event) = self.rx.recv() else {
                break;
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

    /// Processes a single incoming `Event`. It dispatches various event types to the `WindowManager` or other internal handlers.
    ///
    /// # Arguments
    ///
    /// * `event` - The `Event` to process.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is processed successfully, otherwise `Err(Error)`.
    fn process_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Exit => return Ok(()),

            Event::ProcessesLoaded => {
                info!(
                    "{}: === Processes loaded - loading windows ===",
                    function_name!()
                );
                self.initial_scan = false;
                self.window_manager.refresh_displays()?;
                return self.window_manager.set_focused_window();
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

            Event::Command { argv } => self.command(&argv),

            Event::MenuClosed { window_id } => {
                trace!("{}: MenuClosed event: {window_id:?}", function_name!());
            }

            Event::KeyDown { key, modifier } => {
                EventHandler::key_pressed(key, modifier);
            }

            Event::DisplayAdded { display_id } => {
                debug!("{}: Display Added: {display_id:?}", function_name!());
                self.window_manager.display_add(display_id);
            }
            Event::DisplayRemoved { display_id } => {
                debug!("{}: Display Removed: {display_id:?}", function_name!());
                self.window_manager.display_remove(display_id);
            }
            Event::DisplayMoved { display_id } => {
                debug!("{}: Display Moved: {display_id:?}", function_name!());
            }
            Event::DisplayResized { display_id } => {
                debug!("{}: Display Resized: {display_id:?}", function_name!());
            }
            Event::DisplayConfigured { display_id } => {
                debug!("{}: Display Configured: {display_id:?}", function_name!());
            }
            Event::DisplayChanged => {
                debug!("{}: Display Changed", function_name!());
                _ = self.window_manager.reorient_focus();
            }

            Event::SpaceChanged => {
                debug!("{}: Space Changed", function_name!());
                _ = self.window_manager.reorient_focus();
            }
            Event::SystemWoke { msg } => {
                debug!("{}: system woke: {msg:?}", function_name!());
            }

            _ => self.window_manager.process_event(event)?,
        }
        Ok(())
    }

    /// Handles a key press event. It determines the modifier mask and logs the key and modifier.
    ///
    /// # Arguments
    ///
    /// * `key` - The key code of the pressed key.
    /// * `eventflags` - The `CGEventFlags` representing active modifiers.
    fn key_pressed(key: i64, eventflags: CGEventFlags) {
        // Normal key, left, right.
        const MASK_ALT: [u64; 3] = [0x0008_0000, 0x0000_0020, 0x0000_0040];
        const MASK_SHIFT: [u64; 3] = [0x0002_0000, 0x0000_0002, 0x0000_0004];
        const MASK_CMD: [u64; 3] = [0x0010_0000, 0x0000_0008, 0x0000_0010];
        const MASK_CTRL: [u64; 3] = [0x0004_0000, 0x0000_0001, 0x0000_2000];

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

        debug!("KeyDown: {modifier} {key}");
    }

    /// Handles the event when a window gains focus. It checks if the owning application is frontmost
    /// and then delegates to the `WindowManager` to process the focus change.
    ///
    /// # Arguments
    ///
    /// * `window_id` - The ID of the window that gained focus.
    fn window_focused(&mut self, window_id: WinID) {
        debug!("{}: {}", function_name!(), window_id);
        if let Some(window) = self.window_manager.find_window(window_id) {
            if !window.app().is_frontmost() {
                return;
            }

            self.window_manager.window_focused(&window);
        } else {
            warn!(
                "{}: window_manager_add_lost_focused_event",
                function_name!()
            );
            // TODO:
            // window_manager_add_lost_focused_event(&g_window_manager, window_id);
        }
    }

    /// Retrieves a window ID in a specified direction relative to a `current_window_id` within a `WindowPane`.
    ///
    /// # Arguments
    ///
    /// * `direction` - The direction (e.g., "west", "east", "first", "last").
    /// * `current_window_id` - The ID of the current window.
    /// * `panel` - A reference to the `WindowPane` to search within.
    ///
    /// # Returns
    ///
    /// `Some(WinID)` with the found window's ID, otherwise `None`.
    fn get_window_in_direction(
        direction: &str,
        current_window_id: WinID,
        strip: &WindowPane,
    ) -> Option<WinID> {
        let index = strip.index_of(current_window_id).ok()?;
        match direction {
            "west" => (index > 0)
                .then(|| strip.get(index - 1).ok())
                .flatten()
                .and_then(|panel| panel.top()),
            "east" => (index < strip.len() - 1)
                .then(|| strip.get(index + 1).ok())
                .flatten()
                .and_then(|panel| panel.top()),
            "first" => strip.first().ok().and_then(|panel| panel.top()),
            "last" => strip.last().ok().and_then(|panel| panel.top()),
            "north" => match strip.get(index).ok()? {
                Panel::Single(window_id) => Some(window_id),
                Panel::Stack(stack) => stack
                    .iter()
                    .enumerate()
                    .find(|(_, window_id)| current_window_id == **window_id)
                    .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                    .copied(),
            },
            "south" => match strip.get(index).ok()? {
                Panel::Single(window_id) => Some(window_id),
                Panel::Stack(stack) => stack
                    .iter()
                    .enumerate()
                    .find(|(_, window_id)| current_window_id == **window_id)
                    .and_then(|(index, _)| {
                        (index < stack.len() - 1)
                            .then(|| stack.get(index + 1))
                            .flatten()
                    })
                    .copied(),
            },
            dir => {
                error!("{}: Unhandled direction {dir}", function_name!());
                None
            }
        }
    }

    /// Handles the "focus" command, moving focus to a window in a specified direction.
    ///
    /// # Arguments
    ///
    /// * `argv` - A slice of strings representing the command arguments (e.g., [`east`]).
    /// * `current_window` - A reference to the currently focused `Window`.
    /// * `panel` - A reference to the active `WindowPane`.
    ///
    /// # Returns
    ///
    /// `Some(WinID)` with the ID of the newly focused window, otherwise `None`.
    fn command_move_focus(
        &self,
        argv: &[String],
        current_window: &Window,
        strip: &WindowPane,
    ) -> Option<WinID> {
        let direction = argv.first()?;

        EventHandler::get_window_in_direction(direction, current_window.id(), strip).inspect(
            |window_id| {
                let window = self.window_manager.find_window(*window_id);
                if let Some(window) = window {
                    window.focus_with_raise();
                }
            },
        )
    }

    /// Handles the "swap" command, swapping the positions of the current window with another window in a specified direction.
    ///
    /// # Arguments
    ///
    /// * `argv` - A slice of strings representing the command arguments (e.g., [`west`]).
    /// * `current_window` - A reference to the currently focused `Window`.
    /// * `panel` - A reference to the active `WindowPane`.
    /// * `bounds` - The `CGRect` representing the bounds of the display.
    ///
    /// # Returns
    ///
    /// `Some(Window)` with the window that was swapped with, otherwise `None`.
    fn command_swap_focus(
        &self,
        argv: &[String],
        current_window: &Window,
        panel: &WindowPane,
        display_bounds: &CGRect,
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
            CGPoint::new(
                display_bounds.size.width - current_window.frame().size.width,
                0.0,
            )
        } else {
            panel
                .get(new_index)
                .ok()
                .and_then(|panel| panel.top())
                .and_then(|window_id| self.window_manager.find_window(window_id))?
                .frame()
                .origin
        };
        current_window.reposition(origin.x, origin.y, display_bounds);
        if index < new_index {
            (index..new_index).for_each(|idx| panel.swap(idx, idx + 1));
        } else {
            (new_index..index)
                .rev()
                .for_each(|idx| panel.swap(idx, idx + 1));
        }
        Some(window)
    }

    /// Handles various "window" commands, such as focus, swap, center, resize, and manage.
    ///
    /// # Arguments
    ///
    /// * `argv` - A slice of strings representing the command arguments.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the command is processed successfully, otherwise `Err(Error)`.
    fn command_windows(&mut self, argv: &[String]) -> Result<()> {
        let empty = String::new();
        let Some(window) = self
            .window_manager
            .focused_window
            .and_then(|window_id| self.window_manager.find_window(window_id))
            .filter(Window::is_eligible)
        else {
            warn!("{}: No window focused.", function_name!());
            return Ok(());
        };

        let active_display = self.window_manager.active_display()?;
        let active_panel = active_display.active_panel(self.main_cid)?;
        let display_bounds = self.window_manager.current_display_bounds()?;

        let window_id = window.id();
        if window.managed() && active_panel.index_of(window_id).is_err() {
            self.window_manager.reorient_focus()?;
        }

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
                    &display_bounds,
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
                    window.reposition(frame.origin.x, 0.0, &display_bounds);
                    window.resize(
                        frame.size.width,
                        display_bounds.size.height,
                        &display_bounds,
                    );
                    active_panel.append(window.id());
                    window.manage(true);
                }
            }

            "stack" => {
                if !window.managed() {
                    return Ok(());
                }
                active_panel.stack(window.id())?;
            }

            "unstack" => {
                if !window.managed() {
                    return Ok(());
                }
                active_panel.unstack(window.id())?;
            }

            _ => (),
        }
        self.window_manager.reshuffle_around(&window)
    }

    /// Dispatches a command based on the first argument (e.g., "window", "quit").
    ///
    /// # Arguments
    ///
    /// * `argv` - A vector of strings representing the command and its arguments.
    fn command(&mut self, argv: &[String]) {
        if let Some(first) = argv.first() {
            match first.as_ref() {
                "window" => {
                    _ = self
                        .command_windows(&argv[1..])
                        .inspect_err(|err| warn!("{}: {err}", function_name!()));
                }
                "quit" => self.quit.store(true, std::sync::atomic::Ordering::Relaxed),
                _ => warn!("{}: Unhandled command: {argv:?}", function_name!()),
            }
        }
    }

    /// Handles a mouse down event. It finds the window at the click point, reshuffles if necessary,
    /// and stores the clicked window and location.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse down occurred.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is handled successfully, otherwise `Err(Error)`.
    fn mouse_down(&mut self, point: &CGPoint) -> Result<()> {
        debug!("{}: {point:?}", function_name!());
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

    /// Handles a mouse up event. Currently, this function does nothing except logging.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse up occurred.
    fn mouse_up(&mut self, point: &CGPoint) {
        debug!("{}: {point:?}", function_name!());
    }

    /// Handles a mouse dragged event. Currently, this function does nothing except logging.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse was dragged.
    fn mouse_dragged(&self, point: &CGPoint) {
        debug!("{}: {point:?}", function_name!());

        if self.window_manager.mission_control_is_active() {
            #[warn(clippy::needless_return)]
            return;
        }
    }

    /// Handles a mouse moved event. If focus-follows-mouse is enabled, it attempts to focus the window under the cursor.
    /// It also handles child windows like sheets and drawers.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` where the mouse moved to.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is handled successfully or if focus-follows-mouse is disabled, otherwise `Err(Error)`.
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
                for item in get_array_values(&window_list) {
                    let mut child_wid: WinID = 0;
                    unsafe {
                        if !CFNumber::value(
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
                    let Some(child_window) = self.window_manager.find_window(child_wid) else {
                        warn!(
                            "{}: Unable to find child window {child_wid}.",
                            function_name!()
                        );
                        continue;
                    };

                    let Ok(role) = window.role() else {
                        warn!("{}: finding role for {window_id}", function_name!(),);
                        continue;
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

    /// Finds a window at a given screen point using `SkyLight` API.
    ///
    /// # Arguments
    ///
    /// * `point` - A reference to the `CGPoint` representing the screen coordinate.
    ///
    /// # Returns
    ///
    /// `Ok(Window)` with the found window if successful, otherwise `Err(Error)`.
    fn find_window_at_point(&self, point: &CGPoint) -> Result<Window> {
        let mut window_id: WinID = 0;
        let mut window_conn_id: ConnID = 0;
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
                &mut window_conn_id,
            )
        };
        if self.main_cid == window_conn_id {
            unsafe {
                SLSFindWindowAndOwner(
                    self.main_cid,
                    window_id,
                    -1,
                    0,
                    point,
                    &mut window_point,
                    &mut window_id,
                    &mut window_conn_id,
                )
            };
        }
        self.window_manager
            .find_window(window_id)
            .ok_or(Error::other(format!(
                "{}: could not find a window at {point:?}",
                function_name!()
            )))
    }
}
