use arc_swap::ArcSwap;
use core::ptr::NonNull;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{NSEvent, NSEventPhase, NSEventType, NSTouch, NSTouchPhase};
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

use crate::commands::bind_window_command_target;
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

/// How long to suppress scroll wheel events after a vertical swipe gesture,
/// covering macOS momentum scroll that continues after finger lift.
const VERTICAL_GESTURE_SCROLL_SUPPRESS: Duration = Duration::from_millis(1200);

const SWIPE_THRESHOLD: f64 = 0.001;
const GESTURE_MINIMAL_FINGERS: usize = 3;

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputEventMaskPolicy {
    mouse_move: bool,
    mouse_buttons: bool,
    mouse_drag: bool,
    scroll: bool,
    gesture: bool,
    key_down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InputMaskTransition {
    previous: u64,
    next: u64,
}

impl InputMaskTransition {
    pub(super) fn changed(self) -> bool {
        self.previous != self.next
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RunningInputMask(u64);

impl RunningInputMask {
    pub(super) fn new(config: &Config) -> Self {
        Self(InputEventMaskPolicy::from_config(config).mask())
    }

    pub(super) fn transition(self, config: &Config) -> InputMaskTransition {
        InputMaskTransition {
            previous: self.0,
            next: InputEventMaskPolicy::from_config(config).mask(),
        }
    }

    pub(super) fn commit(&mut self, transition: InputMaskTransition) {
        self.0 = transition.next;
    }
}

impl InputEventMaskPolicy {
    fn from_config(config: &Config) -> Self {
        let mouse_resize = config.mouse_resize_modifier().is_some();
        Self {
            mouse_move: config.focus_follows_mouse()
                || config.horizontal_mouse_warp().is_some()
                || mouse_resize,
            // Click-to-reveal is enabled whenever some hidden fraction should
            // force a managed window back into view.
            mouse_buttons: config.window_hidden_ratio() < 1.0,
            // No ECS system consumes MouseDragged; subscribing only burns tap
            // callbacks during drags.
            mouse_drag: false,
            scroll: true,
            gesture: config
                .swipe_gesture_fingers()
                .is_some_and(|fingers| fingers >= GESTURE_MINIMAL_FINGERS),
            key_down: true,
        }
    }

    fn mask(self) -> u64 {
        let mut mask = 0;
        let mut add = |enabled: bool, event: usize| {
            if enabled {
                mask |= 1_u64 << event;
            }
        };
        add(self.mouse_move, CGEventType::MouseMoved.0 as usize);
        add(self.mouse_buttons, CGEventType::LeftMouseDown.0 as usize);
        add(self.mouse_buttons, CGEventType::LeftMouseUp.0 as usize);
        add(self.mouse_buttons, CGEventType::RightMouseDown.0 as usize);
        add(self.mouse_buttons, CGEventType::RightMouseUp.0 as usize);
        add(self.mouse_drag, CGEventType::LeftMouseDragged.0 as usize);
        add(self.mouse_drag, CGEventType::RightMouseDragged.0 as usize);
        add(self.scroll, CGEventType::ScrollWheel.0 as usize);
        add(self.gesture, NSEventType::Gesture.0);
        add(self.key_down, CGEventType::KeyDown.0 as usize);
        mask
    }
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
    /// Timestamp of the last swipe gesture event. Scroll wheel events
    /// are suppressed for a short window after this to prevent the OS from
    /// scrolling windows underneath (including momentum scroll after finger lift).
    last_swipe_time: Option<Instant>,
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
            last_swipe_time: None,
            _pin: PhantomPinned,
        }
    }

    /// Starts the input handler by creating and enabling a `CGEventTap`. It also sets up a cleanup hook.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the event tap is created and started successfully, otherwise `Err(Error)`.
    pub(super) fn start(self) -> Result<PinnedInputHandler> {
        let event_mask = InputEventMaskPolicy::from_config(&self.config).mask();

        let mut pinned = Box::pin(self);
        let this = unsafe { NonNull::new_unchecked(pinned.as_mut().get_unchecked_mut()) }.as_ptr();
        unsafe {
            (*this).tap_port = CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                event_mask,
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
        // Suppress scroll events shortly after a swipe gesture to prevent
        // the OS from scrolling windows underneath, including momentum scroll events
        // that arrive after finger lift.
        if self
            .last_swipe_time
            .is_some_and(|t| t.elapsed() < VERTICAL_GESTURE_SCROLL_SUPPRESS)
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
            let (gesture_began, physical_ended, momentum_began, gesture_ended) =
                NSEvent::eventWithCGEvent(event)
                    .as_deref()
                    .map(|event| scroll_gesture_lifecycle(event.phase(), event.momentumPhase()))
                    .unwrap_or_default();
            let h_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis2,
            );
            let v_delta = CGEvent::double_value_field(
                Some(event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
            );

            // Vertical workspace switching when vertical modifier is also held.
            // Don't set last_vertical_gesture here: the suppress timer is for
            // trackpad momentum scroll, not discrete wheel ticks.
            if combined_match && v_delta.abs() > 0.001 {
                _ = events.send(Event::VerticalScrollTick { delta: v_delta });
                return true;
            }

            // Exact AppKit phases are available for trackpads. Mouse wheels
            // and other phase-less devices continue to use the ECS inactivity
            // fallback instead.
            if base_match && gesture_began {
                _ = events.send(Event::TouchpadDown);
            }
            if base_match && physical_ended {
                _ = events.send(Event::TouchpadPhysicalUp);
            }
            if base_match && momentum_began {
                _ = events.send(Event::TouchpadMomentumStart);
            }

            // If we have any horizontal delta, or if there's only vertical delta, use it.
            let delta = if h_delta.abs() > 0.001 {
                h_delta
            } else if v_delta.abs() > 0.001 {
                v_delta
            } else {
                0.0
            };

            let has_delta = delta.abs() > 0.001;
            if has_delta {
                _ = events.send(Event::Scroll { delta });
            }
            if base_match && gesture_ended {
                _ = events.send(Event::TouchpadUp);
            }
            if has_delta
                || (base_match
                    && (gesture_began || physical_ended || momentum_began || gesture_ended))
            {
                return true; // Intercept: don't let the window scroll
            }
        }
        false
    }

    /// Handles swipe gesture events. Routes to horizontal `Swipe` or vertical
    /// `VerticalSwipe` based on axis dominance. Returns true to intercept the event.
    fn handle_swipe(&mut self, event: &CGEvent) -> bool {
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
        if phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled)
            && let Some(events) = &self.events
        {
            _ = events.send(Event::TouchpadUp);
            return false;
        }

        let fingers = ns_event.allTouches();
        if !gesture_should_intercept(Some(configured_fingers), fingers.len()) {
            return false;
        }
        if fingers.iter().any(|f| f.phase() == NSTouchPhase::Began)
            && let Some(events) = &self.events
        {
            _ = events.send(Event::TouchpadDown);
        }

        if fingers.len() < GESTURE_MINIMAL_FINGERS {
            return false;
        }

        if fingers.iter().all(|f| f.phase() != NSTouchPhase::Began)
            && let Some(prev) = &self.finger_position
        {
            // Match touches by identity rather than relying on NSSet
            // iteration order, which is not guaranteed to be stable.
            let (x_deltas, y_deltas): (Vec<f64>, Vec<f64>) = fingers
                .iter()
                .filter_map(|current| {
                    let id = current.identity();
                    prev.iter()
                        .find(|p| {
                            let p_id = p.identity();
                            let equal: bool = unsafe { msg_send![&*p_id, isEqual: &*id] };
                            equal
                        })
                        .map(|p| {
                            let dx = p.normalizedPosition().x - current.normalizedPosition().x;
                            let dy = p.normalizedPosition().y - current.normalizedPosition().y;
                            (dx, dy)
                        })
                })
                .unzip();

            if let Some(events) = &self.events {
                let x_sum: f64 = x_deltas.iter().sum();
                let y_sum: f64 = y_deltas.iter().sum();

                if x_sum.abs() >= y_sum.abs() {
                    // Horizontal dominant: use existing swipe path
                    if x_deltas.iter().all(|p| p.abs() > SWIPE_THRESHOLD) {
                        _ = events.send(Event::Swipe {
                            delta: x_sum,
                            fingers: x_deltas.len(),
                        });
                        self.last_swipe_time = Some(Instant::now());
                    }
                } else if y_deltas.iter().all(|p| p.abs() > SWIPE_THRESHOLD) {
                    if !self.config.swipe_vertical() {
                        // Do not intercept the vertical swipe
                        return false;
                    }
                    // Vertical dominant: send vertical swipe, intercept the event
                    _ = events.send(Event::VerticalSwipe {
                        delta: y_sum,
                        fingers: y_deltas.len(),
                    });
                    self.last_swipe_time = Some(Instant::now());
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
                self.config.find_keybind(keycode, mask)
            })
            .and_then(bind_window_command_target)
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

fn scroll_gesture_lifecycle(
    phase: NSEventPhase,
    momentum_phase: NSEventPhase,
) -> (bool, bool, bool, bool) {
    // Momentum belongs to the physical gesture that preceded it. Treating its
    // begin as a new gesture would recapture the paging stop and allow a
    // second hop from one finger movement.
    let began = phase.contains(NSEventPhase::Began);
    let physical_ended = phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled);
    let momentum_began = momentum_phase.contains(NSEventPhase::Began);
    let momentum_ended = momentum_phase.intersects(NSEventPhase::Ended | NSEventPhase::Cancelled);
    (began, physical_ended, momentum_began, momentum_ended)
}

fn get_modifiers(eventflags: CGEventFlags) -> Modifiers {
    const MODIFIER_MASKS: [(Modifiers, u64); 9] = [
        (Modifiers::LALT, 0x0000_0020),
        (Modifiers::RALT, 0x0000_0040),
        (Modifiers::LSHIFT, 0x0000_0002),
        (Modifiers::RSHIFT, 0x0000_0004),
        (Modifiers::LCMD, 0x0000_0008),
        (Modifiers::RCMD, 0x0000_0010),
        (Modifiers::LCTRL, 0x0000_0001),
        (Modifiers::RCTRL, 0x0000_2000),
        (Modifiers::FN, 0x0080_0000),
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
    const NX_DEVICEFNKEYMASK: u64 = 0x0080_0000;

    fn manual_config() -> Config {
        Config::try_from(
            r#"
[options]
focus_follows_mouse = false
mouse_follows_focus = false

[swipe.scroll]
modifier = "alt"

[bindings]
"#,
        )
        .unwrap()
    }

    fn feature_config() -> Config {
        Config::try_from(
            r"
[options]
focus_follows_mouse = true
window_hidden_ratio = 1.0

[swipe.gesture]
fingers_count = 3

[bindings]
",
        )
        .unwrap()
    }

    #[test]
    fn manual_use_case_avoids_move_drag_and_gesture_callbacks() {
        let config = manual_config();
        let policy = InputEventMaskPolicy::from_config(&config);
        assert!(!policy.mouse_move);
        assert!(!policy.mouse_drag);
        assert!(!policy.gesture);
        assert!(policy.mouse_buttons, "click-to-reveal remains enabled");
        assert!(policy.scroll);
        assert!(policy.key_down);
    }

    #[test]
    fn feature_flags_add_only_required_mouse_and_gesture_events() {
        let config = feature_config();
        let policy = InputEventMaskPolicy::from_config(&config);
        assert!(policy.mouse_move);
        assert!(!policy.mouse_buttons);
        assert!(!policy.mouse_drag);
        assert!(policy.gesture);
    }

    #[test]
    fn running_mask_reconciles_enable_and_disable_transitions() {
        let manual = manual_config();
        let features = feature_config();
        let mut running = RunningInputMask::new(&manual);

        let enabled = running.transition(&features);
        assert!(enabled.changed());
        running.commit(enabled);
        assert_eq!(
            running.0,
            InputEventMaskPolicy::from_config(&features).mask()
        );
        assert_ne!(
            running.0 & (1_u64 << CGEventType::MouseMoved.0),
            0,
            "enabling focus-follows-mouse must add mouse movement"
        );
        assert_ne!(
            running.0 & (1_u64 << NSEventType::Gesture.0),
            0,
            "enabling gestures must add gesture callbacks"
        );

        let disabled = running.transition(&manual);
        assert!(disabled.changed());
        running.commit(disabled);
        assert_eq!(running.0, InputEventMaskPolicy::from_config(&manual).mask());
        assert_eq!(running.0 & (1_u64 << CGEventType::MouseMoved.0), 0);
        assert_eq!(running.0 & (1_u64 << NSEventType::Gesture.0), 0);
        assert_ne!(running.0 & (1_u64 << CGEventType::ScrollWheel.0), 0);
        assert_ne!(running.0 & (1_u64 << CGEventType::KeyDown.0), 0);
    }

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
    fn fn_with_other_modifiers() {
        let flags = NX_DEVICEFNKEYMASK | NX_DEVICELCMDKEYMASK | NX_DEVICELALTKEYMASK;
        assert_eq!(
            get_modifiers(CGEventFlags(flags)),
            Modifiers::FN | Modifiers::LCMD | Modifiers::LALT
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
        assert_eq!(get_modifiers(CGEventFlags(0x0080_0000)), Modifiers::FN);
    }

    #[test]
    fn only_intercepts_the_explicitly_configured_gesture() {
        assert!(!gesture_should_intercept(None, 3));
        assert!(!gesture_should_intercept(Some(2), 2));
        assert!(gesture_should_intercept(Some(3), 3));
        assert!(!gesture_should_intercept(Some(4), 3));
        assert!(gesture_should_intercept(Some(4), 4));
    }

    #[test]
    fn native_scroll_lifecycle_covers_touch_and_momentum_phases() {
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::Began, NSEventPhase::None),
            (true, false, false, false)
        );
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::Ended, NSEventPhase::None),
            (false, true, false, false)
        );
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::None, NSEventPhase::Began),
            (false, false, true, false),
            "momentum must continue the existing physical gesture"
        );
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::None, NSEventPhase::Ended),
            (false, false, false, true)
        );
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::Ended, NSEventPhase::Began),
            (false, true, true, false),
            "momentum beginning in the same event must keep the gesture active"
        );
        assert_eq!(
            scroll_gesture_lifecycle(NSEventPhase::Ended, NSEventPhase::Changed),
            (false, true, false, false),
            "physical end must not end the full gesture while momentum continues"
        );
    }
}
