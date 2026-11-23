//! Command-line configuration powered by clap.

use std::fmt;
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

/// Headless Android AOAv2 USB OTG mouse, keyboard, hardware specs, and telemetry bridge.
#[derive(Parser)]
#[command(
    name = "android-web-otg",
    version,
    about = "Browser-based Android AOAv2 USB OTG mouse/keyboard bridge with USB Hardware Specs and local telemetry."
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
    #[arg(long, value_parser = parse_hex_id)]
    pub vid: Option<u16>,

    /// Match USB product ID in hex (for example: 2008).
    #[arg(long, value_parser = parse_hex_id)]
    pub pid: Option<u16>,

    /// USB vendor ID in hex to skip (repeatable, e.g. --ignore-vid 148f).
    #[arg(long = "ignore-vid", value_parser = parse_hex_id)]
    pub ignore_vid: Vec<u16>,

    /// Optional bearer / X-Auth-Token protecting all /api/* and /ws/* routes.
    #[arg(long, env = "ANDROID_WEB_OTG_TOKEN")]
    pub token: Option<String>,

    /// Optional local JSONL file path for persisting telemetry events.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Path to the `adb` binary used for optional screen mirroring and tap/swipe control.
    #[arg(long, env = "ANDROID_WEB_OTG_ADB", default_value = "adb")]
    pub adb_path: String,

    /// Run as a stdio MCP (Model Context Protocol) server instead of launching HTTP/UI.
    #[arg(long)]
    pub mcp: bool,

    /// Once per newly attached USB phone, automatically wake and type ANDROID_WEB_OTG_UNLOCK_CODE.
    #[arg(long)]
    pub auto_unlock: bool,

    /// Wake gesture sent before typing ANDROID_WEB_OTG_UNLOCK_CODE during --auto-unlock.
    #[arg(long, value_enum, default_value_t = WakeMethod::Swipe)]
    pub unlock_wake: WakeMethod,
}

impl fmt::Debug for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cli")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("serial", &self.serial)
            .field("vid", &self.vid)
            .field("pid", &self.pid)
            .field("ignore_vid", &self.ignore_vid)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("log_file", &self.log_file)
            .field("adb_path", &self.adb_path)
            .field("mcp", &self.mcp)
            .field("auto_unlock", &self.auto_unlock)
            .field("unlock_wake", &self.unlock_wake)
            .finish()
    }
}

/// Minimum accepted length for `--token` / `ANDROID_WEB_OTG_TOKEN`.
pub const MIN_API_TOKEN_CHARS: usize = 32;

/// Reject an empty or too-short `--token` / `ANDROID_WEB_OTG_TOKEN` before the process binds.
pub fn validate_api_token(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--token / ANDROID_WEB_OTG_TOKEN must not be empty or whitespace.".into());
    }
    if trimmed.chars().count() < MIN_API_TOKEN_CHARS {
        return Err(format!(
            "--token / ANDROID_WEB_OTG_TOKEN must be at least {MIN_API_TOKEN_CHARS} characters. Generate one with: openssl rand -hex 32"
        ));
    }
    Ok(())
}

/// Parse an optional hex USB vendor/product ID (with or without `0x` prefix).
pub fn parse_hex_id(raw: &str) -> Result<u16, String> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")).unwrap_or(trimmed);
    u16::from_str_radix(hex, 16).map_err(|_| format!("Invalid hex USB ID: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_usb_ids() {
        assert_eq!(parse_hex_id("0e8d"), Ok(0x0E8D));
        assert_eq!(parse_hex_id("0x18D1"), Ok(0x18D1));
        assert_eq!(parse_hex_id(" 0X2008 "), Ok(0x2008));
        assert!(parse_hex_id("zzzz").is_err());
        assert!(parse_hex_id("").is_err());
    }

    #[test]
    fn api_token_must_be_at_least_32_characters() {
        let err = validate_api_token(&"a".repeat(31)).expect_err("31 characters must be rejected");
        assert!(err.contains("32"), "{err}");
        assert!(err.contains("openssl rand -hex 32"), "{err}");
        assert!(validate_api_token(&"b".repeat(32)).is_ok());
        assert!(validate_api_token(&format!("  {}  ", "c".repeat(32))).is_ok());
        let empty = validate_api_token("   ").expect_err("whitespace token must be rejected");
        assert!(empty.contains("whitespace"), "{empty}");
    }

    #[test]
    fn cli_debug_redacts_token() {
        let cli = Cli {
            host: "127.0.0.1".into(),
            port: 8095,
            serial: None,
            vid: None,
            pid: None,
            ignore_vid: vec![],
            token: Some("super-secret-token".into()),
            log_file: None,
            adb_path: "adb".into(),
            mcp: false,
            auto_unlock: false,
            unlock_wake: WakeMethod::Swipe,
        };
        let dbg = format!("{cli:?}");
        assert!(!dbg.contains("super-secret-token"));
        assert!(dbg.contains("[REDACTED]"));
    }
}
