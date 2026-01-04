use log::debug;
use std::fmt::Display;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    InvalidWindow,
    InvalidConfig(String),
    ConfigurationWatcher(String),
    NotFound(String),
    /// Indicates a permission error.
    PermissionDenied(String),
    /// Indicates a problem with input.
    InvalidInput(String),
    /// Represents an I/O error, typically from `std::io::Error`.
    IO(String),
    Generic(String),
}

impl Error {
    /// Creates a new generic error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<S: ToString + Display>(flavor: std::io::ErrorKind, msg: S) -> Self {
        Error::Generic(format!("{flavor}: {msg}"))
    }

    /// Creates a new invalid window error.
    pub fn invalid_window(message: &str) -> Self {
        debug!("{message}");
        Error::InvalidWindow
    }
}

impl Display for Error {
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
    fn from(err: toml::de::Error) -> Self {
        Error::InvalidConfig(format!("{err}"))
    }
}

impl From<notify::Error> for Error {
    fn from(err: notify::Error) -> Self {
        Error::ConfigurationWatcher(format!("{err}"))
    }
}

impl From<bevy::ecs::query::QuerySingleError> for Error {
    fn from(err: bevy::ecs::query::QuerySingleError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<bevy::ecs::query::QueryEntityError> for Error {
    fn from(err: bevy::ecs::query::QueryEntityError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl<T> From<std::sync::mpsc::SendError<T>> for Error {
    fn from(err: std::sync::mpsc::SendError<T>) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<std::sync::mpsc::RecvError> for Error {
    fn from(err: std::sync::mpsc::RecvError) -> Self {
        Error::Generic(format!("{err}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IO(format!("{err}"))
    }
}
