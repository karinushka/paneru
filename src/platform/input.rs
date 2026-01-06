use core::ptr::NonNull;
use log::{error, info};
use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventType, NSTouch, NSTouchPhase};
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::NSSet;
use std::ffi::c_void;
use std::ptr::null_mut;
use stdext::function_name;

use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::util::Cleanuper;

/// `InputHandler` manages low-level input events from the macOS `CGEventTap`.
/// It intercepts keyboard and mouse events, processes gestures, and dispatches them as higher-level `Event`s.
pub(super) struct InputHandler {
    /// The `EventSender` for dispatching input events.
    events: EventSender,
    /// The application `Config` for looking up keybindings.
    config: Config,
    /// An optional `Cleanuper` to manage the unregistration of the event tap.
    cleanup: Option<Cleanuper>,
    /// Stores the previous touch positions for swipe gesture detection.
    finger_position: Option<Retained<NSSet<NSTouch>>>,
    /// The `CFMachPort` representing the `CGEventTap`.
    tap_port: Option<CFRetained<CFMachPort>>,
}

impl InputHandler {
    /// Creates a new `InputHandler` instance.
    ///
    /// # Arguments
    ///
    /// * `events` - An `EventSender` to send input-related events.
    /// * `config` - The `Config` object for looking up keybindings.
    ///
    /// # Returns
    ///
    /// A new `InputHandler`.
    pub(super) fn new(events: EventSender, config: Config) -> Self {
        InputHandler {
            events,
            config,
            cleanup: None,
            finger_position: None,
            tap_port: None,
        }
    }

    /// Starts the input handler by creating and enabling a `CGEventTap`. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    pub(super) fn start(&mut self) -> Result<()> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << NSEventType::Gesture.0)
            | (1 << CGEventType::KeyDown.0);

        unsafe {
            let this = NonNull::new_unchecked(self).as_ptr();
            self.tap_port = CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this.cast(),
            );
            if self.tap_port.is_none() {
                return Err(Error::PermissionDenied(format!(
                    "{}: Can not create EventTap.",
                    function_name!()
                )));
            }

            let (run_loop_source, main_loop) =
                CFMachPort::new_run_loop_source(None, self.tap_port.as_deref(), 0)
                    .zip(CFRunLoop::main())
                    .ok_or(Error::PermissionDenied(format!(
                        "{}: Unable to create run loop source",
                        function_name!()
                    )))?;
            CFRunLoop::add_source(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);

            let port = self.tap_port.clone().unwrap();
            self.cleanup = Some(Cleanuper::new(Box::new(move || {
                info!("{}: Unregistering event_handler", function_name!());
                CFRunLoop::remove_source(&main_loop, Some(&run_loop_source), kCFRunLoopCommonModes);
                CFMachPort::invalidate(&port);
                CGEvent::tap_enable(&port, false);
            })));
        }
        Ok(())
    }

    /// The C-callback function for the `CGEventTap`. It dispatches to the `input_handler` method.
    /// This function is declared as `extern "C-unwind"`.
    ///
    /// # Arguments
    ///
    /// * `_` - The `CGEventTapProxy` (unused).
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event_ref` - A mutable `NonNull` pointer to the `CGEvent`.
    /// * `this` - A raw pointer to the `InputHandler` instance.
    ///
    /// # Returns
    ///
    /// A raw mutable pointer to `CGEvent`. Returns `null_mut()` if the event is intercepted.
    extern "C-unwind" fn callback(
        _: CGEventTapProxy,
        event_type: CGEventType,
        mut event_ref: NonNull<CGEvent>,
        this: *mut c_void,
    ) -> *mut CGEvent {
        match NonNull::new(this).map(|this| unsafe { this.cast::<InputHandler>().as_mut() }) {
            Some(this) => {
                let intercept = this.input_handler(event_type, unsafe { event_ref.as_ref() });
                if intercept {
                    return null_mut();
                }
            }
            _ => error!("Zero passed to Event Handler."),
        }
        unsafe { event_ref.as_mut() }
    }

    /// Handles various input events received from the `CGEventTap` callback. It sends corresponding `Event`s.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The `CGEventType` of the event.
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `true` if the event should be intercepted (not passed further), `false` otherwise.
    fn input_handler(&mut self, event_type: CGEventType, event: &CGEvent) -> bool {
        let result = match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                info!("{}: Tap Disabled", function_name!());
                if let Some(port) = &self.tap_port {
                    CGEvent::tap_enable(port, true);
                }
                Ok(())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseDown { point })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseUp { point })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseDragged { point })
            }
            CGEventType::MouseMoved => {
                let point = CGEvent::location(Some(event));
                self.events.send(Event::MouseMoved { point })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                let eventflags = CGEvent::flags(Some(event));
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, eventflags);
            }
            _ => self.handle_swipe(event),
        };
        if let Err(err) = result {
            error!("{}: error sending event: {err}", function_name!());
            // The socket is dead, so no use trying to send to it.
            // Trigger cleanup destructor, unregistering the handler.
            self.cleanup = None;
        }
        // Do not intercept this event, let it fall through.
        false
    }

    /// Handles swipe gesture events.
    /// It calculates the delta of the swipe and sends a `Swipe` event.
    ///
    /// # Arguments
    ///
    /// * `event` - A reference to the `CGEvent`.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event is processed successfully, otherwise `Err(Error)`.
    fn handle_swipe(&mut self, event: &CGEvent) -> Result<()> {
        const GESTURE_MINIMAL_FINGERS: usize = 3;
        let Some(ns_event) = NSEvent::eventWithCGEvent(event) else {
            return Err(Error::InvalidInput(format!(
                "{}: Unable to convert {event:?} to NSEvent.",
                function_name!()
            )));
        };
        if ns_event.r#type() != NSEventType::Gesture {
            return Ok(());
        }
        let fingers = ns_event.allTouches();
        if fingers.len() < GESTURE_MINIMAL_FINGERS {
            return Ok(());
        }

        if fingers.iter().all(|f| f.phase() != NSTouchPhase::Began)
            && let Some(prev) = &self.finger_position
        {
            let deltas = prev
                .iter()
                .zip(&fingers)
                .map(|(p, c)| p.normalizedPosition().x - c.normalizedPosition().x)
                .collect::<Vec<_>>();
            _ = self.events.send(Event::Swipe { deltas });
        }
        self.finger_position = Some(fingers);
        Ok(())
    }

    /// Handles key press events. It determines the modifier mask and attempts to find a matching keybinding in the configuration.
    /// If a binding is found, it sends a `Command` event and intercepts the key press.
    ///
    /// # Arguments
    ///
    /// * `keycode` - The key code of the pressed key.
    /// * `eventflags` - The `CGEventFlags` representing active modifiers.
    ///
    /// # Returns
    ///
    /// `true` if the key press was handled and should be intercepted, `false` otherwise.
    fn handle_keypress(&self, keycode: i64, eventflags: CGEventFlags) -> bool {
        const MODIFIER_MASKS: [[u64; 3]; 4] = [
            // Normal key, left, right.
            [0x0008_0000, 0x0000_0020, 0x0000_0040], // Alt
            [0x0002_0000, 0x0000_0002, 0x0000_0004], // Shift
            [0x0010_0000, 0x0000_0008, 0x0000_0010], // Command
            [0x0004_0000, 0x0000_0001, 0x0000_2000], // Control
        ];
        let mask = MODIFIER_MASKS
            .iter()
            .enumerate()
            .filter_map(|(bitshift, modifier)| {
                modifier
                    .iter()
                    .any(|mask| *mask == (eventflags.0 & mask))
                    .then_some(1 << bitshift)
            })
            .reduce(|acc, mask| acc + mask)
            .unwrap_or(0);

        let keycode = keycode.try_into().ok();
        keycode
            .and_then(|keycode| self.config.find_keybind(keycode, mask))
            .and_then(|command| {
                self.events
                    .send(Event::Command { command })
                    .inspect_err(|err| error!("{}: Error sending command: {err}", function_name!()))
                    .ok()
            })
            .is_some()
    }
}
