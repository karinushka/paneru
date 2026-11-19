//! The argv encoding of a [`Command`]: `["window", "focus", "east"]`.
//!
//! This is the wire format of the `send-cmd` socket protocol and the shape the
//! TOML `[bindings]` keys are split into, so parsing and formatting live
//! together here and are checked against each other by round-trip tests.

use crate::types::commands::{
    Command, DesktopAssignTarget, DesktopOperation, DesktopPinTarget, DesktopTarget, Direction,
    MouseMove, MoveFocus, Operation, ResizeDirection, parse_virtual_workspace_number,
};

/// Why an argv vector is not a command. Consumers wrap this in their own error
/// type; the message is already user-facing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(String);

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn invalid(argv: &[&str]) -> Self {
        Self(format!("invalid command '{argv:?}'"))
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

/// Parses a command argument vector into a [`Command`] (e.g. `["window",
/// "focus", "east"]`).
///
/// # Errors
///
/// Returns [`ParseError`] if `argv` is not a recognized command encoding.
pub fn parse_command(argv: &[&str]) -> Result<Command> {
    let command = *argv.first().unwrap_or(&"");
    Ok(match command {
        "printstate" => Command::PrintState,
        "window" => Command::Window(parse_operation(&argv[1..])?),
        "desktop" => Command::Desktop(parse_desktop_operation(&argv[1..])?),
        "mouse" => Command::Mouse(parse_mouse_move(&argv[1..])?),
        "quit" => Command::Quit,
        "restart" => Command::Restart,
        "showmenu" if argv.len() == 1 => Command::MenuBar(true),
        "hidemenu" if argv.len() == 1 => Command::MenuBar(false),
        "overview" if argv.len() == 1 => Command::Overview,
        _ => return Err(ParseError::new(format!("unhandled command '{argv:?}'"))),
    })
}

/// Parses a window operation (e.g. `["focus", "east"]`).
fn parse_operation(argv: &[&str]) -> Result<Operation> {
    let command = *argv.first().unwrap_or(&"");
    let err = || ParseError::invalid(argv);
    let argument = || argv.get(1).ok_or_else(err).copied();

    Ok(match command {
        // The bindings key splits on `_`, so `window_focus_unmanaged` arrives
        // here as ["focus", "unmanaged"] and the suffix tells us the variant.
        "focus" => match argument()? {
            "unmanaged" => Operation::FocusUnmanaged,
            "managed" => Operation::FocusManaged,
            direction => Operation::Focus(Direction::parse_positional(direction)?),
        },
        "raise" => match argument()? {
            "floating" => Operation::RaiseFloating,
            _ => return Err(err()),
        },
        "togglefloatlayer" => Operation::ToggleFloatingLayer,
        "swap" => Operation::Swap(Direction::parse(argument()?)?),
        "center" => Operation::Center,
        "resize" => Operation::Resize(
            argv.get(1)
                .map_or(Ok(ResizeDirection::Grow), |arg| ResizeDirection::parse(arg))?,
        ),
        "grow" => Operation::Resize(ResizeDirection::Grow),
        "shrink" => Operation::Resize(ResizeDirection::Shrink),
        // `window_vertical_resize` / `_grow` / `_shrink`, plus the explicit
        // `window_vertical_resize_shrink` mirroring `window_resize_shrink`.
        "vertical" => Operation::ResizeVertical(match argv.get(1).copied() {
            None | Some("resize") => argv
                .get(2)
                .map_or(Ok(ResizeDirection::Grow), |arg| ResizeDirection::parse(arg))?,
            Some(arg) => ResizeDirection::parse(arg)?,
        }),
        "fullwidth" => Operation::FullWidth,
        "manage" => Operation::Manage,
        "equalize" => Operation::Equalize,
        "balance" => Operation::Balance,
        "stack" => Operation::Stack(true),
        "unstack" => Operation::Stack(false),
        "consume" => Operation::Consume,
        "expel" => Operation::Expel,
        "tabbeddisplay" => Operation::ToggleTabbedDisplay,
        "nextdisplay" => Operation::ToNextDisplay(MoveFocus::Follow),
        "nextdisplaysend" => Operation::ToNextDisplay(MoveFocus::Stay),
        "displaymove" => Operation::ToDisplay(parse_display_id(argument()?)?, MoveFocus::Follow),
        "displaysend" => Operation::ToDisplay(parse_display_id(argument()?)?, MoveFocus::Stay),
        "snap" => Operation::Snap,
        "copyrule" => Operation::CopyRule,
        // The `virtual*` verbs take either a direction or a workspace number,
        // and `num` variants that only take a number.
        "virtual" => virtual_target(argument()?, Operation::Virtual, Operation::VirtualNumber)?,
        // No numeric form: "focus a stack neighbor, else switch" only makes
        // sense paired with a direction.
        "virtualfocus" => Operation::FocusOrVirtual(Direction::parse(argument()?)?),
        "virtualnum" => Operation::VirtualNumber(parse_virtual_workspace_number(argument()?)?),
        "virtualadd" => Operation::VirtualAdd,
        "virtualmove" => virtual_target(
            argument()?,
            |direction| Operation::VirtualMove(direction, MoveFocus::Follow),
            |index| Operation::VirtualMoveNumber(index, MoveFocus::Follow),
        )?,
        "virtualmovenum" => Operation::VirtualMoveNumber(
            parse_virtual_workspace_number(argument()?)?,
            MoveFocus::Follow,
        ),
        "virtualsend" => virtual_target(
            argument()?,
            |direction| Operation::VirtualMove(direction, MoveFocus::Stay),
            |index| Operation::VirtualMoveNumber(index, MoveFocus::Stay),
        )?,
        "virtualsendnum" => Operation::VirtualMoveNumber(
            parse_virtual_workspace_number(argument()?)?,
            MoveFocus::Stay,
        ),
        _ => return Err(err()),
    })
}

/// Resolves a `virtual*` argument that may be a direction or a 1-based number.
fn virtual_target(
    target: &str,
    directional: impl Fn(Direction) -> Operation,
    numbered: impl Fn(u32) -> Operation,
) -> Result<Operation> {
    if target.parse::<u32>().is_ok() {
        Ok(numbered(parse_virtual_workspace_number(target)?))
    } else {
        Ok(directional(Direction::parse(target)?))
    }
}

fn parse_display_id(input: &str) -> Result<u32> {
    input
        .parse::<u32>()
        .map_err(|_| ParseError::new(format!("invalid display id '{input}'")))
}

/// Parses a mouse command (e.g. `["nextdisplay"]`).
fn parse_mouse_move(argv: &[&str]) -> Result<MouseMove> {
    match (argv.first().copied().unwrap_or(""), argv.get(1).copied()) {
        ("nextdisplay", _) => Ok(MouseMove::ToNextDisplay),
        ("display", Some(id)) => Ok(MouseMove::ToDisplay(parse_display_id(id)?)),
        _ => Err(ParseError::new(format!("invalid mouse command '{argv:?}'"))),
    }
}

fn parse_desktop_number(input: &str) -> Result<u32> {
    let number = input
        .parse::<u32>()
        .map_err(|_| ParseError::new(format!("invalid desktop target '{input}'")))?;
    if number == 0 {
        return Err(ParseError::new("desktop numbers start at 1"));
    }
    Ok(number)
}

fn parse_desktop_target(argv: &[&str]) -> Result<DesktopTarget> {
    match argv {
        ["id", raw] => {
            let space_id = raw
                .parse::<u64>()
                .ok()
                .filter(|&id| id > 0)
                .ok_or_else(|| ParseError::new(format!("invalid space id '{raw}'")))?;
            Ok(DesktopTarget::SpaceId(space_id))
        }
        [target] => parse_desktop_step_or_number(target, None),
        [target, "display", disp] => {
            parse_desktop_step_or_number(target, Some(parse_display_id(disp)?))
        }
        _ => Err(ParseError::invalid(argv)),
    }
}

fn parse_desktop_step_or_number(target: &str, display: Option<u32>) -> Result<DesktopTarget> {
    if target.bytes().all(|b| b.is_ascii_digit()) && !target.is_empty() {
        Ok(DesktopTarget::Number {
            number: parse_desktop_number(target)?,
            display,
        })
    } else {
        Ok(DesktopTarget::Direction {
            direction: Direction::parse(target)?,
            display,
        })
    }
}

fn parse_desktop_operation(argv: &[&str]) -> Result<DesktopOperation> {
    let verb = *argv.first().unwrap_or(&"");
    let rest = argv.get(1..).unwrap_or(&[]);
    Ok(match verb {
        "focus" => DesktopOperation::Focus(parse_desktop_target(rest)?),
        "move" => DesktopOperation::MoveWindow {
            target: parse_desktop_target(rest)?,
            move_focus: MoveFocus::Follow,
        },
        "send" => DesktopOperation::MoveWindow {
            target: parse_desktop_target(rest)?,
            move_focus: MoveFocus::Stay,
        },
        "create" => match rest {
            [] => DesktopOperation::Create { focus: false },
            ["focus"] => DesktopOperation::Create { focus: true },
            _ => return Err(ParseError::invalid(argv)),
        },
        "destroy" => match rest {
            [] => DesktopOperation::Destroy {
                target: None,
                empty_only: false,
            },
            ["empty"] => DesktopOperation::Destroy {
                target: None,
                empty_only: true,
            },
            [target_args @ .., "empty"] => DesktopOperation::Destroy {
                target: Some(parse_desktop_target(target_args)?),
                empty_only: true,
            },
            target_args => DesktopOperation::Destroy {
                target: Some(parse_desktop_target(target_args)?),
                empty_only: false,
            },
        },
        "reorder" => DesktopOperation::Reorder(parse_desktop_target(rest)?),
        "swap" => DesktopOperation::Swap(parse_desktop_target(rest)?),
        "pin" => match rest {
            ["all"] => DesktopOperation::Pin(DesktopPinTarget::All),
            _ => DesktopOperation::Pin(DesktopPinTarget::Desktop(parse_desktop_target(rest)?)),
        },
        "unpin" if rest.is_empty() => DesktopOperation::Unpin,
        "assign" => {
            let (target_args, pid) = match rest {
                [head @ .., "pid", pid_str] => {
                    let pid = pid_str
                        .parse::<i32>()
                        .ok()
                        .filter(|&p| p > 0)
                        .ok_or_else(|| ParseError::new(format!("invalid pid '{pid_str}'")))?;
                    (head, Some(pid))
                }
                all => (all, None),
            };
            let target = match target_args {
                ["all"] => DesktopAssignTarget::All,
                ["this"] => DesktopAssignTarget::This,
                ["none"] => DesktopAssignTarget::None,
                other => DesktopAssignTarget::Desktop(parse_desktop_target(other)?),
            };
            DesktopOperation::Assign { target, pid }
        }
        _ => return Err(ParseError::invalid(argv)),
    })
}

impl Command {
    /// The argv encoding of this command, as understood by [`parse_command`].
    ///
    /// [`Command::Lua`] and [`Command::Layout`] have no encoding — they are
    /// only ever issued in-process — and yield `None`.
    #[must_use]
    pub fn to_argv(&self) -> Option<Vec<String>> {
        let argv = match self {
            Command::Window(operation) => {
                let mut argv = vec!["window".to_string()];
                argv.extend(operation.to_argv());
                argv
            }
            Command::Desktop(operation) => {
                let mut argv = vec!["desktop".to_string()];
                operation.push_argv(&mut argv);
                argv
            }
            Command::Mouse(MouseMove::ToNextDisplay) => {
                vec!["mouse".to_string(), "nextdisplay".to_string()]
            }
            Command::Mouse(MouseMove::ToDisplay(id)) => {
                vec!["mouse".to_string(), "display".to_string(), id.to_string()]
            }
            Command::Quit => vec!["quit".to_string()],
            Command::Restart => vec!["restart".to_string()],
            Command::PrintState => vec!["printstate".to_string()],
            Command::MenuBar(enabled) => {
                vec![if *enabled { "showmenu" } else { "hidemenu" }.to_string()]
            }
            Command::Overview => vec!["overview".to_string()],
            Command::Lua(_) | Command::Layout(_) => return None,
        };
        Some(argv)
    }
}

impl Operation {
    /// The argv tail following `window`, e.g. `["focus", "east"]`.
    fn to_argv(&self) -> Vec<String> {
        let owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_string()).collect();
        match self {
            Operation::Focus(direction) => vec!["focus".to_string(), direction.token()],
            Operation::Swap(direction) => vec!["swap".to_string(), direction.token()],
            Operation::Center => owned(&["center"]),
            Operation::Resize(direction) => owned(&["resize", direction.token()]),
            Operation::ResizeVertical(direction) => {
                owned(&["vertical", "resize", direction.token()])
            }
            // `SetWidth` comes from window rules, not from a command line; it has
            // no argv verb, so encode it as the equivalent full-width toggle.
            Operation::SetWidth(_) | Operation::FullWidth => owned(&["fullwidth"]),
            Operation::ToNextDisplay(MoveFocus::Follow) => owned(&["nextdisplay"]),
            Operation::ToNextDisplay(MoveFocus::Stay) => owned(&["nextdisplaysend"]),
            Operation::ToDisplay(id, MoveFocus::Follow) => {
                vec!["displaymove".to_string(), id.to_string()]
            }
            Operation::ToDisplay(id, MoveFocus::Stay) => {
                vec!["displaysend".to_string(), id.to_string()]
            }
            Operation::Equalize => owned(&["equalize"]),
            Operation::Balance => owned(&["balance"]),
            Operation::Manage => owned(&["manage"]),
            Operation::Stack(true) => owned(&["stack"]),
            Operation::Stack(false) => owned(&["unstack"]),
            Operation::Consume => owned(&["consume"]),
            Operation::Expel => owned(&["expel"]),
            Operation::ToggleTabbedDisplay => owned(&["tabbeddisplay"]),
            Operation::Snap => owned(&["snap"]),
            Operation::Virtual(direction) => vec!["virtual".to_string(), direction.token()],
            Operation::FocusOrVirtual(direction) => {
                vec!["virtualfocus".to_string(), direction.token()]
            }
            Operation::VirtualNumber(index) => {
                vec!["virtualnum".to_string(), (index + 1).to_string()]
            }
            Operation::VirtualAdd => owned(&["virtualadd"]),
            Operation::VirtualMove(direction, MoveFocus::Follow) => {
                vec!["virtualmove".to_string(), direction.token()]
            }
            Operation::VirtualMove(direction, MoveFocus::Stay) => {
                vec!["virtualsend".to_string(), direction.token()]
            }
            Operation::VirtualMoveNumber(index, MoveFocus::Follow) => {
                vec!["virtualmovenum".to_string(), (index + 1).to_string()]
            }
            Operation::VirtualMoveNumber(index, MoveFocus::Stay) => {
                vec!["virtualsendnum".to_string(), (index + 1).to_string()]
            }
            Operation::FocusUnmanaged => owned(&["focus", "unmanaged"]),
            Operation::FocusManaged => owned(&["focus", "managed"]),
            Operation::RaiseFloating => owned(&["raise", "floating"]),
            Operation::ToggleFloatingLayer => owned(&["togglefloatlayer"]),
            Operation::CopyRule => owned(&["copyrule"]),
        }
    }
}

impl DesktopTarget {
    fn push_argv(&self, out: &mut Vec<String>) {
        match self {
            DesktopTarget::SpaceId(id) => {
                out.push("id".to_string());
                out.push(id.to_string());
            }
            DesktopTarget::Direction { direction, display } => {
                out.push(direction.token());
                if let Some(disp) = display {
                    out.push("display".to_string());
                    out.push(disp.to_string());
                }
            }
            DesktopTarget::Number { number, display } => {
                out.push(number.to_string());
                if let Some(disp) = display {
                    out.push("display".to_string());
                    out.push(disp.to_string());
                }
            }
        }
    }
}

impl DesktopOperation {
    fn push_argv(&self, out: &mut Vec<String>) {
        match self {
            DesktopOperation::Focus(target) => {
                out.push("focus".to_string());
                target.push_argv(out);
            }
            DesktopOperation::MoveWindow { target, move_focus } => {
                let verb = match move_focus {
                    MoveFocus::Follow => "move",
                    MoveFocus::Stay => "send",
                };
                out.push(verb.to_string());
                target.push_argv(out);
            }
            DesktopOperation::Create { focus } => {
                out.push("create".to_string());
                if *focus {
                    out.push("focus".to_string());
                }
            }
            DesktopOperation::Destroy { target, empty_only } => {
                out.push("destroy".to_string());
                if let Some(target) = target {
                    target.push_argv(out);
                }
                if *empty_only {
                    out.push("empty".to_string());
                }
            }
            DesktopOperation::Reorder(target) => {
                out.push("reorder".to_string());
                target.push_argv(out);
            }
            DesktopOperation::Swap(target) => {
                out.push("swap".to_string());
                target.push_argv(out);
            }
            DesktopOperation::Pin(DesktopPinTarget::All) => {
                out.push("pin".to_string());
                out.push("all".to_string());
            }
            DesktopOperation::Pin(DesktopPinTarget::Desktop(target)) => {
                out.push("pin".to_string());
                target.push_argv(out);
            }
            DesktopOperation::Unpin => out.push("unpin".to_string()),
            DesktopOperation::Assign { target, pid } => {
                out.push("assign".to_string());
                match target {
                    DesktopAssignTarget::All => out.push("all".to_string()),
                    DesktopAssignTarget::This => out.push("this".to_string()),
                    DesktopAssignTarget::None => out.push("none".to_string()),
                    DesktopAssignTarget::Desktop(t) => t.push_argv(out),
                }
                if let Some(pid) = pid {
                    out.push("pid".to_string());
                    out.push(pid.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(command: &Command) -> Command {
        let argv = command.to_argv().expect("command should encode to argv");
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        parse_command(&borrowed).unwrap_or_else(|err| panic!("re-parsing {argv:?}: {err}"))
    }

    #[test]
    fn every_operation_round_trips_through_argv() {
        let operations = [
            Operation::Focus(Direction::East),
            Operation::Focus(Direction::Nth(2)),
            Operation::Swap(Direction::West),
            Operation::Center,
            Operation::Resize(ResizeDirection::Shrink),
            Operation::ResizeVertical(ResizeDirection::Grow),
            Operation::ResizeVertical(ResizeDirection::Shrink),
            Operation::FullWidth,
            Operation::ToNextDisplay(MoveFocus::Follow),
            Operation::ToNextDisplay(MoveFocus::Stay),
            Operation::ToDisplay(3, MoveFocus::Follow),
            Operation::ToDisplay(3, MoveFocus::Stay),
            Operation::Equalize,
            Operation::Balance,
            Operation::Manage,
            Operation::Stack(true),
            Operation::Stack(false),
            Operation::Consume,
            Operation::Expel,
            Operation::ToggleTabbedDisplay,
            Operation::Snap,
            Operation::Virtual(Direction::First),
            Operation::FocusOrVirtual(Direction::North),
            Operation::FocusOrVirtual(Direction::South),
            Operation::VirtualNumber(2),
            Operation::VirtualMove(Direction::East, MoveFocus::Follow),
            Operation::VirtualMove(Direction::East, MoveFocus::Stay),
            Operation::VirtualMoveNumber(0, MoveFocus::Follow),
            Operation::VirtualMoveNumber(0, MoveFocus::Stay),
            Operation::FocusUnmanaged,
            Operation::FocusManaged,
            Operation::RaiseFloating,
            Operation::ToggleFloatingLayer,
            Operation::CopyRule,
        ];

        for operation in operations {
            let command = Command::Window(operation.clone());
            let reparsed = round_trip(&command);
            assert_eq!(
                format!("{reparsed:?}"),
                format!("{command:?}"),
                "argv round-trip changed {operation:?}"
            );
        }
    }

    #[test]
    fn global_commands_round_trip() {
        for command in [
            Command::Quit,
            Command::Restart,
            Command::PrintState,
            Command::MenuBar(true),
            Command::MenuBar(false),
            Command::Overview,
            Command::Mouse(MouseMove::ToNextDisplay),
            Command::Mouse(MouseMove::ToDisplay(3)),
        ] {
            assert_eq!(
                format!("{:?}", round_trip(&command)),
                format!("{command:?}")
            );
        }
    }

    #[test]
    fn display_commands_need_a_numeric_id() {
        assert!(parse_command(&["window", "displaymove"]).is_err());
        assert!(parse_command(&["window", "displaysend", "left"]).is_err());
        assert!(parse_command(&["mouse", "display"]).is_err());
        assert_eq!(
            parse_command(&["window", "displaymove", "2"]).unwrap(),
            Command::Window(Operation::ToDisplay(2, MoveFocus::Follow))
        );
    }

    #[test]
    fn lua_commands_have_no_argv_encoding() {
        assert!(Command::Lua(1).to_argv().is_none());
    }

    #[test]
    fn window_numbers_are_one_based() {
        assert_eq!(
            Direction::parse_positional("1"),
            Ok(Direction::Nth(0)),
            "the first window is number 1"
        );
        assert!(Direction::parse_positional("0").is_err());
    }

    #[test]
    fn desktop_commands_round_trip_and_reject_zero_numbers() {
        let operations = [
            DesktopOperation::Focus(DesktopTarget::Direction {
                direction: Direction::East,
                display: None,
            }),
            DesktopOperation::Focus(DesktopTarget::Number {
                number: 2,
                display: None,
            }),
            DesktopOperation::Focus(DesktopTarget::Number {
                number: 3,
                display: Some(2),
            }),
            DesktopOperation::Focus(DesktopTarget::SpaceId(42)),
            DesktopOperation::MoveWindow {
                target: DesktopTarget::Direction {
                    direction: Direction::West,
                    display: None,
                },
                move_focus: MoveFocus::Follow,
            },
            DesktopOperation::MoveWindow {
                target: DesktopTarget::Number {
                    number: 1,
                    display: None,
                },
                move_focus: MoveFocus::Stay,
            },
            DesktopOperation::Create { focus: false },
            DesktopOperation::Create { focus: true },
            DesktopOperation::Destroy {
                target: None,
                empty_only: false,
            },
            DesktopOperation::Destroy {
                target: None,
                empty_only: true,
            },
            DesktopOperation::Destroy {
                target: Some(DesktopTarget::Number {
                    number: 2,
                    display: None,
                }),
                empty_only: true,
            },
            DesktopOperation::Reorder(DesktopTarget::Number {
                number: 2,
                display: None,
            }),
            DesktopOperation::Swap(DesktopTarget::Direction {
                direction: Direction::East,
                display: None,
            }),
            DesktopOperation::Pin(DesktopPinTarget::All),
            DesktopOperation::Pin(DesktopPinTarget::Desktop(DesktopTarget::Number {
                number: 2,
                display: None,
            })),
            DesktopOperation::Unpin,
            DesktopOperation::Assign {
                target: DesktopAssignTarget::All,
                pid: None,
            },
            DesktopOperation::Assign {
                target: DesktopAssignTarget::This,
                pid: Some(1234),
            },
            DesktopOperation::Assign {
                target: DesktopAssignTarget::None,
                pid: None,
            },
        ];

        for op in operations {
            let cmd = Command::Desktop(op);
            assert_eq!(round_trip(&cmd), cmd);
        }

        assert!(parse_command(&["desktop", "focus", "0"]).is_err());
        assert!(parse_command(&["desktop", "focus"]).is_err());
        assert!(parse_command(&["desktop", "unpin", "extra"]).is_err());
    }

    #[test]
    fn invalid_commands_are_rejected() {
        assert!(parse_command(&["definitely", "not", "a", "command"]).is_err());
        assert!(parse_command(&["window", "focus"]).is_err());
        assert!(parse_command(&["window", "swap", "3"]).is_err());
        assert!(parse_command(&["showmenu", "extra"]).is_err());
        assert!(parse_command(&["hidemenu", "extra"]).is_err());
    }
}
