use arc_swap::ArcSwap;
use core::ptr::NonNull;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventType, NSTouch, NSTouchPhase};
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::NSSet;
use scopeguard::ScopeGuard;
use std::ffi::c_void;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock};
use stdext::function_name;
use tracing::{error, info};

use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::Modifiers;

/// The currently active set of passthrough keybindings, shared lock-free with
/// the `CGEvent` tap callback thread via `ArcSwap`.
static FOCUSED_PASSTHROUGH: LazyLock<ArcSwap<Vec<(u8, Modifiers)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the passthrough keybinding set that the event tap checks on every
/// key-down. Called from the ECS thread on focus change and config reload.
pub fn set_focused_passthrough(keys: Vec<(u8, Modifiers)>) {
    FOCUSED_PASSTHROUGH.store(Arc::new(keys));
}

/// `InputHandler` manages low-level input events from the macOS `CGEventTap`.
/// It intercepts keyboard and mouse events, processes gestures, and dispatches them as higher-level `Event`s.
pub(super) struct InputHandler {
    /// The `EventSender` for dispatching input events.
    events: Option<EventSender>,
    /// The application `Config` for looking up keybindings.
    config: Config,
    /// Stores the previous touch positions for swipe gesture detection.
    finger_position: Option<Retained<NSSet<NSTouch>>>,
    /// The `CFMachPort` representing the `CGEventTap`.
    tap_port: Option<CFRetained<CFMachPort>>,
    // Prevents from being Unpin automatically
    _pin: PhantomPinned,
}

pub(super) type PinnedInputHandler =
    ScopeGuard<Pin<Box<InputHandler>>, Box<dyn FnOnce(Pin<Box<InputHandler>>)>>;

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
            events: Some(events),
            config,
            finger_position: None,
            tap_port: None,
            _pin: PhantomPinned,
        }
    }

    /// Starts the input handler by creating and enabling a `CGEventTap`. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    pub(super) fn start(self) -> Result<PinnedInputHandler> {
        let mouse_event_mask = (1 << CGEventType::MouseMoved.0)
            | (1 << CGEventType::LeftMouseDown.0)
            | (1 << CGEventType::LeftMouseUp.0)
            | (1 << CGEventType::LeftMouseDragged.0)
            | (1 << CGEventType::RightMouseDown.0)
            | (1 << CGEventType::RightMouseUp.0)
            | (1 << CGEventType::RightMouseDragged.0)
            | (1 << CGEventType::ScrollWheel.0)
            | (1 << NSEventType::Gesture.0)
            | (1 << CGEventType::KeyDown.0);

        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        unsafe {
            (*this).tap_port = CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mouse_event_mask,
                Some(Self::callback),
                this.cast(),
            );
        };
        if pinned.tap_port.is_none() {
            return Err(Error::PermissionDenied(format!(
                "{}: Can not create EventTap.",
                function_name!()
            )));
        }

        let (run_loop_source, main_loop) =
            CFMachPort::new_run_loop_source(None, pinned.tap_port.as_deref(), 0)
                .zip(CFRunLoop::main())
                .ok_or(Error::PermissionDenied(format!(
                    "{}: Unable to create run loop source",
                    function_name!()
                )))?;
        let loop_mode = unsafe { kCFRunLoopCommonModes };
        CFRunLoop::add_source(&main_loop, Some(&run_loop_source), loop_mode);

        let port = pinned
            .tap_port
            .clone()
            .ok_or(Error::PermissionDenied(format!(
                "{}: invalid tap port.",
                function_name!()
            )))?;
        Ok(scopeguard::guard(
            pinned,
            Box::new(move |_: Pin<Box<Self>>| {
                info!("Unregistering event_handler");
                CFRunLoop::remove_source(&main_loop, Some(&run_loop_source), loop_mode);
                CFMachPort::invalidate(&port);
                CGEvent::tap_enable(&port, false);
            }),
        ))
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
        if let Some(this) =
            NonNull::new(this).map(|this| unsafe { this.cast::<InputHandler>().as_mut() })
        {
            let intercept = this.input_handler(event_type, unsafe { event_ref.as_ref() });
            if intercept {
                return null_mut();
            }
        } else {
            error!("Zero passed to Event Handler.");
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
        let Some(events) = &self.events else {
            return false;
        };
        let result = match event_type {
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                info!("Tap Disabled");
                if let Some(port) = &self.tap_port {
                    CGEvent::tap_enable(port, true);
                }
                Ok(())
            }
            CGEventType::LeftMouseDown | CGEventType::RightMouseDown => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseDown { point })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseUp { point })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseDragged { point })
            }
            CGEventType::MouseMoved => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseMoved { point })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                let eventflags = CGEvent::flags(Some(event));
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, eventflags);
            }
            CGEventType::ScrollWheel => {
                return self.handle_scroll_wheel(event);
            }
            _ => self.handle_swipe(event),
        };
        if let Err(err) = result {
            error!("error sending event: {err}");
            // The socket is dead, so no use trying to send to it.
            // Trigger cleanup destructor, unregistering the handler.
            self.events = None;
        }
        // Do not intercept this event, let it fall through.
        false
    }

    /// Handles scroll wheel events. If configured modifier is held, it transforms the scroll into a swipe event.
    fn handle_scroll_wheel(&mut self, event: &CGEvent) -> bool {
        let flags = CGEvent::flags(Some(event));
        let modifiers = get_modifiers(flags);

        let target_modifier = self.config.swipe_scroll_modifier();

        // Only intercept if the configured modifier is held.
        // Intersect to allow for left OR right modifiers to match the target modifier.
        if (modifiers & target_modifier).is_empty() {
            return false;
        }

        if let Some(events) = &self.events {
            // Horizontal scroll delta is Axis 2, Vertical is Axis 1.
            // On macOS, horizontal scroll (holding shift + wheel or side-tilt) is often preferred for swiping between strips.
            // We use the continuous scroll delta (Fixed) for smoother swiping.
            let h_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
            );
            let v_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
            );

            // If we have any horizontal delta, or if there's only vertical delta, use it.
            let delta = if h_delta.abs() > 0.001 {
                h_delta
            } else if v_delta.abs() > 0.001 {
                v_delta
            } else {
                0.0
            };

            if delta.abs() > 0.001 {
                _ = events.send(Event::Scroll { delta });
                return true; // Intercept: don't let the window scroll
            }
        }
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
        const SWIPE_THRESHOLD: f64 = 0.001;
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
            // Match touches by identity rather than relying on NSSet
            // iteration order, which is not guaranteed to be stable.
            let deltas = fingers
                .iter()
                .filter_map(|current| {
                    let id = current.identity();
                    prev.iter()
                        .find(|p| {
                            let p_id = p.identity();
                            let equal: bool = unsafe { msg_send![&*p_id, isEqual: &*id] };
                            equal
                        })
                        .map(|p| p.normalizedPosition().x - current.normalizedPosition().x)
                })
                .collect::<Vec<_>>();
            if deltas.iter().all(|p| p.abs() > SWIPE_THRESHOLD)
                && let Some(events) = &self.events
            {
                _ = events.send(Event::Swipe { deltas });
            }
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
        let Some(events) = &self.events else {
            return false;
        };

        let mask = get_modifiers(eventflags);

        // On a native fullscreen space, keybindings are still intercepted so
        // that paneru can actively switch back to the previous workspace.
        // Non-paneru keys pass through naturally (find_keybind returns None).

        let keycode = keycode.try_into().ok();
        keycode
            .and_then(|keycode| {
                let passthrough = FOCUSED_PASSTHROUGH.load();
                if passthrough
                    .iter()
                    .any(|(c, m)| *c == keycode && m.matches(mask))
                {
                    return None;
                }
                self.config.find_keybind(keycode, mask)
            })
            .and_then(|command| {
                events
                    .send(Event::Command { command })
                    .inspect_err(|err| error!("Error sending command: {err}"))
                    .ok()
            })
            .is_some()
    }
}

fn get_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const MODIFIER_MASKS: [(Modifiers, u64); 8] = [
        (Modifiers::LALT, 0x0000_0020),
        (Modifiers::RALT, 0x0000_0040),
        (Modifiers::LSHIFT, 0x0000_0002),
        (Modifiers::RSHIFT, 0x0000_0004),
        (Modifiers::LCMD, 0x0000_0008),
        (Modifiers::RCMD, 0x0000_0010),
        (Modifiers::LCTRL, 0x0000_0001),
        (Modifiers::RCTRL, 0x0000_2000),
    ];
    let mut mask = Modifiers::empty();
    for (modifier, m) in MODIFIER_MASKS {
        if eventflags.0 & m != 0 {
            mask |= modifier;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
    const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

    #[test]
    fn no_modifiers() {
        assert_eq!(get_modifiers(CGEventFlags(0)), Modifiers::empty());
    }

    #[test]
    fn single_left_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELALTKEYMASK)),
            Modifiers::LALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELSHIFTKEYMASK)),
            Modifiers::LSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCMDKEYMASK)),
            Modifiers::LCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICELCTLKEYMASK)),
            Modifiers::LCTRL
        );
    }

    #[test]
    fn single_right_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERALTKEYMASK)),
            Modifiers::RALT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERSHIFTKEYMASK)),
            Modifiers::RSHIFT
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCMDKEYMASK)),
            Modifiers::RCMD
        );
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICERCTLKEYMASK)),
            Modifiers::RCTRL
        );
    }

    #[test]
    fn both_sides_of_same_modifier() {
        let flags = NX_DEVICELALTKEYMASK | NX_DEVICERALTKEYMASK;
        assert_eq!(get_modifiers(CGEventFlags(flags)), Modifiers::ALT);
    }

    #[test]
    fn multiple_different_modifiers() {
        let flags = NX_DEVICELCTLKEYMASK | NX_DEVICELALTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCTRL | Modifiers::LALT
        );
    }

    #[test]
    fn mixed_sides_across_groups() {
        let flags = NX_DEVICELCMDKEYMASK | NX_DEVICERALTKEYMASK | NX_DEVICERSHIFTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::LCMD | Modifiers::RALT | Modifiers::RSHIFT
        );
    }

    #[test]
    fn device_independent_flags_ignored() {
        let generic_alt: u64 = 0x0008_0000;
        assert_eq!(get_modifiers(CGEventFlags(generic_alt)), Modifiers::empty());
    }
}
