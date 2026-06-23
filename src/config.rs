//! Runtime configuration, sourced from environment variables.
//!
//! Everything that used to be hardcoded — bind address, port, STUN server, the
//! emulator LAN-IP workaround, log filter — lives here so a deploy is a matter
//! of setting env vars, not editing source. All values have defaults, so the
//! server runs with zero configuration.
//!
//! | Env var                  | Default                          |
//! |--------------------------|----------------------------------|
//! | `WCS_BIND`               | `0.0.0.0`                        |
//! | `WCS_PORT`               | `8090`                           |
//! | `WCS_STUN`               | `stun://stun.l.google.com:19302` |
//! | `WCS_EMULATOR_LAN_IP`    | (unset — workaround disabled)    |
//! | `WCS_STATIC_DIR`         | `templates`                      |
//! | `RUST_LOG`               | `webrtc_camera_server=info,...`  |

use std::net::IpAddr;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the HTTP/WS server to.
    pub bind: IpAddr,
    /// TCP port to listen on.
    pub port: u16,
    /// STUN server URI handed to each `webrtcbin`.
    pub stun_server: String,
    /// When set, ICE candidates containing this LAN IP are duplicated with the
    /// IP rewritten to the Android-emulator host alias (10.0.2.2). `None`
    /// disables the workaround entirely (the production default).
    pub emulator_lan_ip: Option<String>,
    /// Directory served as static files (the test client).
    pub static_dir: String,
}

impl Config {
    /// Load configuration from the environment, applying defaults. Fails only on
    /// values that are present but unparseable (e.g. a non-numeric port), so a
    /// typo is caught at startup rather than silently ignored.
    pub fn from_env() -> Result<Self> {
        let bind = match std::env::var("WCS_BIND") {
            Ok(v) => v
                .parse::<IpAddr>()
                .with_context(|| format!("WCS_BIND is not a valid IP address: {v:?}"))?,
            Err(_) => IpAddr::from([0, 0, 0, 0]),
        };

        let port = match std::env::var("WCS_PORT") {
            Ok(v) => v
                .parse::<u16>()
                .with_context(|| format!("WCS_PORT is not a valid port: {v:?}"))?,
            Err(_) => 8090,
        };

        let stun_server = std::env::var("WCS_STUN")
            .unwrap_or_else(|_| "stun://stun.l.google.com:19302".to_string());

        // Empty string counts as unset so `WCS_EMULATOR_LAN_IP=` disables it.
        let emulator_lan_ip = std::env::var("WCS_EMULATOR_LAN_IP")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let static_dir = std::env::var("WCS_STATIC_DIR").unwrap_or_else(|_| "templates".to_string());

        Ok(Self {
            bind,
            port,
            stun_server,
            emulator_lan_ip,
            static_dir,
        })
    }
}
