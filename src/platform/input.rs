use arc_swap::ArcSwap;
use core::ptr::NonNull;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
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
use std::time::{Duration, Instant};
use stdext::function_name;
use tracing::{error, info};

use crate::commands::Command;
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::platform::Modifiers;

const NX_DEVICEFNKEYMASK: u64 = 0x0080_0100;

/// The currently active set of passthrough keybindings, shared lock-free with
/// the `CGEvent` tap callback thread via `ArcSwap`.
static FOCUSED_PASSTHROUGH: LazyLock<ArcSwap<Vec<(u8, Modifiers)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the passthrough keybinding set that the event tap checks on every
/// key-down. Called from the ECS thread on focus change and config reload.
pub fn set_focused_passthrough(keys: Vec<(u8, Modifiers)>) {
    FOCUSED_PASSTHROUGH.store(Arc::new(keys));
}

/// Keybinds registered by the Lua runtime as `(keycode, modifiers, handler_id)`,
/// shared lock-free with the event tap. Checked before the config bindings so a
/// scripted bind can override a TOML one.
static LUA_KEYBINDS: LazyLock<ArcSwap<Vec<(u8, Modifiers, u32)>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replace the Lua keybind set that the event tap checks on every key-down.
/// Called from the main thread on script load and hot reload.
#[cfg(feature = "lua")]
pub fn set_lua_keybinds(keys: Vec<(u8, Modifiers, u32)>) {
    LUA_KEYBINDS.store(Arc::new(keys));
}

/// How long after a swipe gesture macOS may still be delivering its momentum.
///
/// An upper bound, not a blackout: only events the system has *marked* as
/// momentum are dropped inside it (see [`is_momentum`]), so a deliberate scroll
/// during the window still goes through. The bound exists so a momentum phase
/// that never reports its end cannot silence scrolling indefinitely.
const GESTURE_MOMENTUM_WINDOW: Duration = Duration::from_millis(1200);

/// Whether the system tagged this scroll event as momentum — the coasting macOS
/// synthesises after the fingers leave, rather than anything the user is doing
/// now.
///
/// `kCGScrollWheelEventMomentumPhase` is zero for user-driven scrolling and
/// non-zero while coasting, which is exactly the distinction the suppression
/// wants. Timing alone cannot make it: a swipe that lands the pointer on a new
/// window is immediately followed by the user scrolling *that* window, and to a
/// clock those are indistinguishable from the tail of the gesture.
fn is_momentum(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::ScrollWheelEventMomentumPhase) != 0
}

/// Which way a swipe went.
///
/// The suppression window exists to stop the momentum scroll macOS keeps
/// emitting after a gesture from scrolling whatever sits underneath. Momentum
/// continues along the axis the fingers were already travelling, so that is the
/// only axis needing silence — blanking both meant a horizontal swipe took
/// vertical input, workspace switching included, down with it for over a second.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SwipeAxis {
    Horizontal,
    Vertical,
}

impl SwipeAxis {
    /// The axis a pair of deltas predominantly lies along.
    ///
    /// Ties go to horizontal, matching how [`InputHandler::handle_swipe`]
    /// decides which way a gesture went.
    fn dominant(horizontal: f64, vertical: f64) -> Self {
        if horizontal.abs() >= vertical.abs() {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }
}

/// One finger, as a single gesture event saw it.
///
/// Read out of the `NSTouch` set once per event, because everything below wants
/// these values several times over and the matching in [`InputHandler::handle_swipe`]
/// is quadratic in the number of fingers. Reading `identity` and
/// `normalizedPosition` back through Objective-C inside that loop made the
/// message count quadratic with it, and re-retained an identity object per
/// comparison, on every event of every swipe.
struct Touch {
    /// Opaque per-finger token, compared with `isEqual:` to follow one finger
    /// across events. `NSSet` iteration order is not stable, so position in the
    /// set says nothing.
    identity: Retained<AnyObject>,
    x: f64,
    y: f64,
    /// Whether the finger landed this event, which suppresses delta tracking
    /// until every finger is down.
    began: bool,
}

impl Touch {
    /// Samples the whole set in one pass.
    fn sample(touches: &NSSet<NSTouch>) -> Vec<Self> {
        touches
            .iter()
            .map(|touch| {
                let position = touch.normalizedPosition();
                Self {
                    identity: touch.identity(),
                    x: position.x,
                    y: position.y,
                    began: touch.phase() == NSTouchPhase::Began,
                }
            })
            .collect()
    }

    /// Whether this and `other` are the same finger.
    fn is(&self, other: &Self) -> bool {
        // SAFETY: `identity` is documented as conforming to `NSObject`, so it
        // answers `isEqual:`.
        unsafe { msg_send![&*self.identity, isEqual: &*other.identity] }
    }
}

const SWIPE_THRESHOLD: f64 = 0.001;
const GESTURE_MINIMAL_FINGERS: usize = 3;

/// `InputHandler` manages low-level input events from the macOS `CGEventTap`.
/// It intercepts keyboard and mouse events, processes gestures, and dispatches them as higher-level `Event`s.
pub(super) struct InputHandler {
    /// The `EventSender` for dispatching input events.
    events: Option<EventSender>,
    /// The application `Config` for looking up keybindings.
    config: Config,
    /// The previous event's fingers, for swipe gesture detection.
    finger_position: Option<Vec<Touch>>,
    /// The `CFMachPort` representing the `CGEventTap`.
    tap_port: Option<CFRetained<CFMachPort>>,
    /// When the last swipe gesture happened, and which way it went. Scroll
    /// events *along that axis* are suppressed briefly afterwards, keeping the
    /// momentum that follows finger lift from scrolling what is underneath.
    last_swipe: Option<(Instant, SwipeAxis)>,
    /// The direction the gesture in progress committed to, held until the
    /// fingers lift.
    ///
    /// Deciding this per event instead made a swipe fight itself. Fingers wobble,
    /// so in the middle of a horizontal swipe individual events do come out
    /// vertically dominant — and each one either drove the strip the wrong way or,
    /// with vertical swiping off, fell through to macOS and scrolled whatever was
    /// underneath. Either way that event's horizontal motion was lost, which is
    /// the stutter. A swipe picks a direction once and keeps it.
    gesture_axis: Option<SwipeAxis>,
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
            last_swipe: None,
            gesture_axis: None,
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

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_modifiers(flags);

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
                events.send(Event::MouseDown { point, modifiers })
            }
            CGEventType::LeftMouseUp | CGEventType::RightMouseUp => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseUp { point, modifiers })
            }
            CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseDragged { point, modifiers })
            }
            CGEventType::MouseMoved => {
                let point = CGEvent::location(Some(event));
                events.send(Event::MouseMoved { point, modifiers })
            }
            CGEventType::KeyDown => {
                let keycode =
                    CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
                // handle_keypress can intercept the event, so it may return true.
                return self.handle_keypress(keycode, flags);
            }
            CGEventType::ScrollWheel => {
                return self.handle_scroll_wheel(event);
            }
            // Returns directly: handle_swipe returns bool (intercept flag)
            // rather than Result like the other arms.
            _ => {
                return self.handle_swipe(event);
            }
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
        let h_delta = CGEvent::double_value_field(
            Some(event),
            CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
        );
        let v_delta = CGEvent::double_value_field(
            Some(event),
            CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
        );

        // Drop the momentum macOS keeps synthesising after a gesture, so it does
        // not scroll whatever the swipe just landed on.
        //
        // Three conditions, and all of them earn their place. The event must be
        // *marked* as momentum, so a scroll the user actually performs is never
        // swallowed — which is what used to make a window unusable for over a
        // second after being swiped to. It must run along the axis the gesture
        // travelled, since that is the only direction momentum carries. And it
        // must fall inside the window, so a momentum phase that never reports an
        // end cannot silence that axis forever.
        if let Some((when, axis)) = self.last_swipe
            && when.elapsed() < GESTURE_MOMENTUM_WINDOW
            && axis == SwipeAxis::dominant(h_delta, v_delta)
            && is_momentum(event)
        {
            return true;
        }

        let flags = CGEvent::flags(Some(event));
        let modifiers = get_modifiers(flags);

        let target_modifier = self.config.swipe_scroll_modifier();
        let vertical_mod = self.config.swipe_scroll_vertical_modifier();

        // Check the combined modifier (base + vertical) first, then fall back to
        // base-only. matches() rejects extra modifier groups, so cmd+shift held
        // would fail a cmd-only check. We need to accept both.
        let base_match = target_modifier.matches(modifiers);
        let combined_match =
            vertical_mod.is_some_and(|vm| (target_modifier | vm).matches(modifiers));
        if !combined_match && !base_match {
            return false;
        }

        if let Some(events) = &self.events {
            // Vertical workspace switching when vertical modifier is also held.
            // Don't set last_vertical_gesture here: the suppress timer is for
            // trackpad momentum scroll, not discrete wheel ticks.
            if combined_match && v_delta.abs() > 0.001 {
                _ = events.send(Event::VerticalScrollTick { delta: v_delta });
                return true;
            }

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

    /// Handles swipe gesture events. Routes to horizontal `Swipe` or vertical
    /// `VerticalSwipe` based on axis dominance. Returns true to intercept the event.
    fn handle_swipe(&mut self, event: &CGEvent) -> bool {
        const NS_EVENT_PHASE_ENDED: usize = 1 << 3; // 8
        const NS_EVENT_PHASE_CANCELLED: usize = 1 << 4; // 16

        let Some(configured_fingers) = self
            .config
            .swipe_gesture_fingers()
            .filter(|fingers| *fingers >= GESTURE_MINIMAL_FINGERS)
        else {
            // No valid gesture is configured, so leave native macOS gestures alone.
            return false;
        };

        let Some(ns_event) = NSEvent::eventWithCGEvent(event) else {
            error!("{}: Unable to convert CGEvent to NSEvent", function_name!());
            return false;
        };
        if ns_event.r#type() != NSEventType::Gesture {
            return false;
        }

        // Fingers lifted off touchpad.
        let phase = ns_event.phase();
        if (phase.0 & NS_EVENT_PHASE_CANCELLED != 0 || phase.0 & NS_EVENT_PHASE_ENDED != 0)
            && let Some(events) = &self.events
        {
            _ = events.send(Event::TouchpadUp);
            self.gesture_axis = None;
            self.finger_position = None;
            return false;
        }

        let fingers = Touch::sample(&ns_event.allTouches());
        if !gesture_should_intercept(Some(configured_fingers), fingers.len()) {
            return false;
        }
        if fingers.iter().any(|finger| finger.began) {
            // A fresh gesture: nothing carries over from the last one.
            self.gesture_axis = None;
            if let Some(events) = &self.events {
                _ = events.send(Event::TouchpadDown);
            }
        }

        if fingers.len() < GESTURE_MINIMAL_FINGERS {
            return false;
        }

        if fingers.iter().all(|finger| !finger.began)
            && let Some(prev) = &self.finger_position
        {
            // Match touches by identity rather than relying on NSSet
            // iteration order, which is not guaranteed to be stable.
            let (x_deltas, y_deltas): (Vec<f64>, Vec<f64>) = fingers
                .iter()
                .filter_map(|current| {
                    prev.iter()
                        .find(|previous| previous.is(current))
                        .map(|previous| (previous.x - current.x, previous.y - current.y))
                })
                .unzip();

            let x_sum: f64 = x_deltas.iter().sum();
            let y_sum: f64 = y_deltas.iter().sum();
            let moved = x_sum.abs().max(y_sum.abs()) > SWIPE_THRESHOLD;

            // The first event that actually travels decides the direction; the
            // rest of the gesture follows it.
            let axis = match self.gesture_axis {
                Some(axis) => Some(axis),
                None if moved => {
                    let axis = SwipeAxis::dominant(x_sum, y_sum);
                    self.gesture_axis = Some(axis);
                    Some(axis)
                }
                // Still settling — no direction to commit to yet.
                None => None,
            };

            if let (Some(axis), Some(events)) = (axis, &self.events) {
                // Tested on the travel of the whole hand rather than of every
                // finger. Requiring each one to clear the threshold every event
                // meant a single finger easing off dropped that event outright,
                // and its motion with it, which read as the swipe snagging.
                match axis {
                    SwipeAxis::Horizontal if x_sum.abs() > SWIPE_THRESHOLD => {
                        _ = events.send(Event::Swipe {
                            delta: x_sum,
                            fingers: x_deltas.len(),
                        });
                        self.last_swipe = Some((Instant::now(), SwipeAxis::Horizontal));
                    }
                    SwipeAxis::Vertical if y_sum.abs() > SWIPE_THRESHOLD => {
                        if !self.config.swipe_vertical() {
                            // Committed to a direction this build does not
                            // handle, so the whole gesture goes to macOS rather
                            // than alternating between us and it.
                            return false;
                        }
                        _ = events.send(Event::VerticalSwipe {
                            delta: y_sum,
                            fingers: y_deltas.len(),
                        });
                        self.last_swipe = Some((Instant::now(), SwipeAxis::Vertical));
                    }
                    _ => (),
                }
            }
        }
        self.finger_position = Some(fingers);

        // If we have 3 or more fingers on the trackpad, we intercept the event
        // to prevent it from being interpreted as a scroll by the OS.
        true
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
                // Lua-registered binds take precedence over the TOML config.
                let lua_binds = LUA_KEYBINDS.load();
                if let Some((_, _, id)) = lua_binds
                    .iter()
                    .find(|(c, m, _)| *c == keycode && m.matches(mask))
                {
                    return Some(Command::Lua(*id));
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

fn gesture_should_intercept(configured_fingers: Option<usize>, actual_fingers: usize) -> bool {
    configured_fingers.is_some_and(|configured| {
        configured >= GESTURE_MINIMAL_FINGERS && configured == actual_fingers
    })
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

    // Fn key should be checked for separately, because pressing
    // some keys (i.e. leftarrow) seems to inadvertently toggling it.
    if eventflags.0 == NX_DEVICEFNKEYMASK {
        tracing::debug!("event flags {:#x}", eventflags.0);
        return Modifiers::FN;
    }

    MODIFIER_MASKS
        .iter()
        .fold(Modifiers::empty(), |modifiers, (modifier, mask)| {
            if eventflags.0 & mask != 0 {
                modifiers | *modifier
            } else {
                modifiers
            }
        })
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
    fn fn_modifier() {
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
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
    #[test]
    fn secondary_fn_flag_is_not_ignored() {
        // Ensure we don't accidentally filter out the fn mask as "device independent"
        assert_eq!(
            get_modifiers(CGEventFlags(NX_DEVICEFNKEYMASK)),
            Modifiers::FN
        );
    }

    #[test]
    fn only_intercepts_the_explicitly_configured_gesture() {
        assert!(!gesture_should_intercept(None, 3));
        assert!(!gesture_should_intercept(Some(2), 2));
        assert!(gesture_should_intercept(Some(3), 3));
        assert!(!gesture_should_intercept(Some(4), 3));
        assert!(gesture_should_intercept(Some(4), 4));
    }
}
