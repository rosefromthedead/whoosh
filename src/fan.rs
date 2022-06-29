use std::fs::{read_to_string, write};

use tracing::warn;

use crate::error::Error;

pub struct ControlledFan {
    path_prefix: String,
    initial_mode: u8,
}

impl ControlledFan {
    pub fn new(path_prefix: String) -> Result<Self, Error> {
        let mut enable_path = path_prefix.clone();
        enable_path.push_str("_enable");
        let mode_string = read_to_string(&enable_path)?;
        let initial_mode = mode_string.trim().parse().map_err(Error::InvalidMode)?;

        write(enable_path, b"1\n")?;

        Ok(Self {
            path_prefix,
            initial_mode,
        })
    }

    pub fn get_speed(&self) -> Result<u8, Error> {
        let speed_string = read_to_string(&self.path_prefix)?;
        let speed = speed_string.trim().parse().map_err(Error::InvalidSpeed)?;
        Ok(speed)
    }

    pub fn set_speed(&self, new_speed: u8) -> Result<(), Error> {
        write(&self.path_prefix, format!("{}\n", new_speed))?;
        Ok(())
    }
}

impl Drop for ControlledFan {
    fn drop(&mut self) {
        let mut enable_path = self.path_prefix.clone();
        enable_path.push_str("_enable");
        let res = write(enable_path, format!("{}\n", self.initial_mode).as_bytes());
        if let Err(e) = res {
            warn!(path_prefix = self.path_prefix.as_str(), error = ?e, "failed to reset fan");
        }
    }
}
