//! Command-line configuration powered by clap.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Wake sequence used before typing a preset or manual unlock code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WakeMethod {
    Swipe,
    Space,
    None,
}

impl WakeMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Swipe => "swipe",
            Self::Space => "space",
            Self::None => "none",
        }
    }
}

/// Headless Android AOAv2 USB OTG mouse, keyboard, phone specs, and telemetry bridge.
#[derive(Parser, Debug)]
#[command(
    name = "android-web-otg",
    version,
    about = "Browser-based Android AOAv2 USB OTG mouse/keyboard bridge with Phone Specs and local telemetry."
)]
pub struct Cli {
    /// HTTP bind address (keep 127.0.0.1 unless behind Tailscale/VPN or paired with --token).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP listen port.
    #[arg(long, default_value_t = 8095)]
    pub port: u16,

    /// Match a specific Android USB device serial number.
    #[arg(long)]
    pub serial: Option<String>,

    /// Match USB vendor ID in hex (for example: 0e8d).
    #[arg(long)]
    pub vid: Option<String>,

    /// Match USB product ID in hex (for example: 2008).
    #[arg(long)]
    pub pid: Option<String>,

    /// USB vendor ID in hex to skip (repeatable, e.g. --ignore-vid 148f).
    #[arg(long = "ignore-vid")]
    pub ignore_vid: Vec<String>,

    /// Optional bearer / X-Auth-Token protecting all /api/* and /ws/* routes.
    #[arg(long, env = "ANDROID_WEB_OTG_TOKEN")]
    pub token: Option<String>,

    /// Optional local JSONL file path for persisting telemetry events.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Once per newly attached USB phone, automatically wake and type ANDROID_WEB_OTG_UNLOCK_CODE.
    #[arg(long)]
    pub auto_unlock: bool,

    /// Wake gesture sent before typing ANDROID_WEB_OTG_UNLOCK_CODE during --auto-unlock.
    #[arg(long, value_enum, default_value_t = WakeMethod::Swipe)]
    pub unlock_wake: WakeMethod,

    /// Disable optional ADB polling, screencap, and Android OS specs inspection.
    #[arg(long)]
    pub no_adb: bool,
}

/// Parse an optional hex USB vendor/product ID (with or without `0x` prefix).
pub fn parse_hex_id(raw: &str) -> Result<u16, String> {
    let trimmed = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(trimmed, 16).map_err(|_| format!("Invalid hex USB ID: {raw}"))
}
