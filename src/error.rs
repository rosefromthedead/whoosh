use std::{io::Error as IoError, num::ParseIntError};

use displaydoc::Display;
use toml::de::Error as TomlError;

#[derive(Debug, Display)]
pub enum Error {
    /// The specified hwmon name "{0}" was not found.
    HwmonNameNotFound(String),
    /// The specified sensor (label or index) was not found.
    HwmonSensorNotFound,
    /// One of the curve points defined in the configuration file was invalid.
    InvalidPointSpec,
    /// A sensor reading was not a valid integer.
    InvalidReading(ParseIntError),
    /// A fan mode was not a valid integer.
    InvalidMode(ParseIntError),
    /// A fan speed was not a valid integer.
    InvalidSpeed(ParseIntError),

    /// The configuration file does not contain valid TOML: {0}
    Toml(toml::de::Error),
    /// An I/O error occurred: {0}
    Io(std::io::Error),
}

impl From<TomlError> for Error {
    fn from(e: toml::de::Error) -> Self {
        Error::Toml(e)
    }
}

impl From<IoError> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
