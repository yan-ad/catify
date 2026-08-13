//! Typed configuration loading. Filesystem discovery stays outside domain crates.

use cfy_core::{Error, ErrorKind, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub telemetry: Option<Telemetry>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Telemetry {
    pub enabled: bool,
}

pub fn parse(input: &str) -> Result<Config> {
    toml::from_str(input)
        .map_err(|error| Error::with_source(ErrorKind::Config, "invalid TOML configuration", error))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_minimal_config() {
        let config = super::parse("[telemetry]\nenabled = false").unwrap();
        assert!(!config.telemetry.unwrap().enabled);
    }
}
