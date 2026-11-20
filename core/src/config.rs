//! `devices.json`, written once by `research/discover.py`.

use std::path::PathBuf;

use crate::ayla::DeviceConfig;

/// `%APPDATA%\hvac-control\devices.json` on Windows, `~/.config/hvac-control/devices.json` elsewhere.
pub fn default_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_default().join("hvac-control").join("devices.json")
}

/// The first device in the file.
pub fn load(path: &std::path::Path) -> Result<DeviceConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!("Cannot read {}: {e}. Run research/discover.py first.", path.display())
    })?;
    let devices: Vec<DeviceConfig> =
        serde_json::from_str(&text).map_err(|e| format!("Bad {}: {e}", path.display()))?;
    devices.into_iter().next().ok_or_else(|| format!("No devices in {}", path.display()))
}
