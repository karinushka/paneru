use log::debug;
use std::fmt::Display;

/// A type alias for `std::result::Result` with a custom `Error` type.
pub type Result<T> = std::result::Result<T, Error>;

/// Represents the various types of errors that can occur within the application.
#[derive(Debug)]
pub enum Error {
    /// Indicates an invalid window operation or state.
    InvalidWindow,
    /// Indicates an issue with the application's configuration, with a descriptive message.
    InvalidConfig(String),
    /// Indicates an error during the watching or processing of configuration file changes.
    ConfigurationWatcher(String),
    /// Indicates that a requested item (e.g., a window or space) was not found, with a descriptive message.
    NotFound(String),
    /// Indicates a permission error.
    PermissionDenied(String),
    /// Indicates a problem with input.
    InvalidInput(String),
    /// Represents an I/O error, typically from `std::io::Error`.
    IO(String),
    /// A generic error with a descriptive message.
    Generic(String),
}

impl Error {
    /// Creates a new generic error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<S: ToString + Display>(flavor: std::io::ErrorKind, msg: S) -> Self {
        Error::Generic(format!("{flavor}: {msg}"))
    }

    /// Creates a new `InvalidWindow` error with a debug message.
    ///
    /// # Arguments
    ///
    /// * `message` - A string slice providing additional debug information.
    ///
    /// # Returns
    ///
    /// An `Error::InvalidWindow` instance.
    pub fn invalid_window(message: &str) -> Self {
        debug!("{message}");
        Error::InvalidWindow
    }
}

impl Display for Error {
    /// Formats the `Error` for display, providing a user-friendly error message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Error::InvalidWindow => "Invalid window".to_string(),
            Error::InvalidConfig(msg) => format!("Invalid configuration: {msg}"),
            Error::ConfigurationWatcher(msg) => format!("Watching config file: {msg}"),
            Error::NotFound(msg) => format!("Not found: {msg}"),
            Error::PermissionDenied(msg) => format!("Permission denied: {msg}"),
            Error::InvalidInput(msg) => format!("Invalid input: {msg}"),
            Error::IO(msg) => format!("IO error: {msg}"),
            Error::Generic(msg) => format!("Generic error: {msg}"),
        };
        write!(f, "{msg}")
    }
}

impl From<toml::de::Error> for Error {
    /// Converts a `toml::de::Error` into an `Error::InvalidConfig`.
    fn from(err: toml::de::Error) -> Self {
        Error::InvalidConfig(format!("{err}"))
    }
}

impl From<notify::Error> for Error {
    /// Converts a `notify::Error` into an `Error::ConfigurationWatcher`.
    fn from(err: notify::Error) -> Self {
        Error::ConfigurationWatcher(format!("{err}"))
    }
}

impl From<bevy::ecs::query::QuerySingleError> for Error {
    /// Converts a `bevy::ecs::query::QuerySingleError` into an `Error::Generic`.
    fn from(err: bevy::ecs::query::QuerySingleError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<bevy::ecs::query::QueryEntityError> for Error {
    /// Converts a `bevy::ecs::query::QueryEntityError` into an `Error::Generic`.
    fn from(err: bevy::ecs::query::QueryEntityError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for Error {
    /// Converts a `std::sync::mpsc::SendError<T>` into an `Error::Generic`.
    fn from(err: std::sync::mpsc::SendError<T>) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<std::sync::mpsc::RecvError> for Error {
    /// Converts a `std::sync::mpsc::RecvError` into an `Error::Generic`.
    fn from(err: std::sync::mpsc::RecvError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<std::io::Error> for Error {
    /// Converts a `std::io::Error` into an `Error::IO`.
    fn from(err: std::io::Error) -> Self {
        Error::IO(format!("{err}"))
    }
}
