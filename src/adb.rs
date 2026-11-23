//! Optional ADB inspection, system specs, live screencap, and tap/swipe helpers.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

use crate::telemetry::TelemetryStore;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AdbDeviceInfo {
    pub serial: String,
    pub state: String,
    pub product: String,
    pub model: String,
    pub device: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DisplaySpecs {
    pub width: u32,
    pub height: u32,
    pub density_dpi: Option<u32>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MemorySpecs {
    pub total_mb: Option<u64>,
    pub available_mb: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StorageSpecs {
    pub total_gb: Option<f64>,
    pub used_gb: Option<f64>,
    pub available_gb: Option<f64>,
    pub use_pct: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct BatterySpecs {
    pub level: Option<u8>,
    pub status: Option<String>,
    pub temperature_c: Option<f64>,
    pub powered: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AndroidSpecs {
    pub serial: String,
    pub manufacturer: String,
    pub model: String,
    pub device: String,
    pub android_version: String,
    pub sdk: String,
    pub security_patch: String,
    pub hardware: String,
    pub platform: String,
    pub abi: String,
    pub kernel: String,
    pub display: Option<DisplaySpecs>,
    pub memory: MemorySpecs,
    pub storage_data: Option<StorageSpecs>,
    pub battery: BatterySpecs,
}

/// Parse `adb devices -l` lines into structured entries.
#[must_use]
pub fn parse_adb_devices(output: &str) -> Vec<AdbDeviceInfo> {
    let mut list = Vec::new();
    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        let mut entry = AdbDeviceInfo {
            serial: serial.to_string(),
            state: state.to_string(),
            product: String::new(),
            model: String::new(),
            device: String::new(),
        };
        for tok in parts {
            if let Some((k, v)) = tok.split_once(':') {
                match k {
                    "product" => entry.product = v.to_string(),
                    "model" => entry.model = v.to_string(),
                    "device" => entry.device = v.to_string(),
                    _ => {}
                }
            }
        }
        list.push(entry);
    }
    list
}

/// Parse `/proc/meminfo` for `MemTotal` and `MemAvailable` in MiB.
#[must_use]
pub fn parse_meminfo(text: &str) -> MemorySpecs {
    let mut total_mb = None;
    let mut available_mb = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            if let Some(kb) = parse_kb_field(rest) {
                total_mb = Some((kb as f64 / 1024.0).round() as u64);
            }
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            if let Some(kb) = parse_kb_field(rest) {
                available_mb = Some((kb as f64 / 1024.0).round() as u64);
            }
        }
    }
    MemorySpecs { total_mb, available_mb }
}

fn parse_kb_field(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse::<u64>().ok()
}

/// Parse `df -k /data` output into GiB metrics.
#[must_use]
pub fn parse_df_data(text: &str) -> Option<StorageSpecs> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 2 {
        return None;
    }
    let cols: Vec<&str> = lines.last()?.split_whitespace().collect();
    if cols.len() < 5 {
        return None;
    }
    let total_kb: f64 = cols[1].parse().ok()?;
    let used_kb: f64 = cols[2].parse().ok()?;
    let avail_kb: f64 = cols[3].parse().ok()?;
    let div = 1024.0 * 1024.0;
    Some(StorageSpecs {
        total_gb: Some(((total_kb / div) * 10.0).round() / 10.0),
        used_gb: Some(((used_kb / div) * 10.0).round() / 10.0),
        available_gb: Some(((avail_kb / div) * 10.0).round() / 10.0),
        use_pct: cols[4].to_string(),
    })
}

/// Parse `dumpsys battery` key-value lines.
#[must_use]
pub fn parse_battery(text: &str) -> BatterySpecs {
    let mut level = None;
    let mut status = None;
    let mut temperature_c = None;
    let mut powered_flags = Vec::new();
    for line in text.lines() {
        let Some((k_raw, v_raw)) = line.split_once(':') else { continue };
        let k = k_raw.trim();
        let v = v_raw.trim();
        match k {
            "level" => {
                if let Ok(n) = v.parse::<u8>() {
                    level = Some(n);
                }
            }
            "status" => {
                status = Some(match v {
                    "1" => "Unknown".to_string(),
                    "2" => "Charging".to_string(),
                    "3" => "Discharging".to_string(),
                    "4" => "Not charging".to_string(),
                    "5" => "Full".to_string(),
                    other => other.to_string(),
                });
            }
            "temperature" => {
                if let Ok(t) = v.parse::<f64>() {
                    temperature_c = Some((t).round() / 10.0);
                }
            }
            "AC powered" | "USB powered" | "Wireless powered" if v.eq_ignore_ascii_case("true") => {
                if let Some(kind) = k.split_whitespace().next() {
                    powered_flags.push(kind.to_string());
                }
            }
            _ => {}
        }
    }
    let powered = if !powered_flags.is_empty() {
        Some(powered_flags.join("+"))
    } else if level.is_some() {
        Some("Battery".to_string())
    } else {
        None
    };
    BatterySpecs { level, status, temperature_c, powered }
}

/// Parse `wm size` output into `(width, height)`.
#[must_use]
pub fn parse_wm_size(text: &str) -> Option<(u32, u32)> {
    let mut dims = None;
    for line in text.lines() {
        if let Some((_, rhs)) = line.split_once(':') {
            if let Some((w_str, h_str)) = rhs.trim().split_once('x') {
                if let (Ok(w), Ok(h)) = (w_str.trim().parse::<u32>(), h_str.trim().parse::<u32>()) {
                    if w > 0 && h > 0 {
                        dims = Some((w, h));
                    }
                }
            }
        }
    }
    dims
}

/// Parse `wm density` output into DPI.
#[must_use]
pub fn parse_wm_density(text: &str) -> Option<u32> {
    let mut dpi = None;
    for line in text.lines() {
        if let Some((_, rhs)) = line.split_once("density:") {
            if let Ok(n) = rhs.trim().parse::<u32>() {
                dpi = Some(n);
            }
        }
    }
    dpi
}

fn parse_getprop(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('[') {
            if let Some((key, val_part)) = rest.split_once("]: [") {
                if let Some(val) = val_part.strip_suffix(']') {
                    map.insert(key.to_string(), val.to_string());
                }
            }
        }
    }
    map
}

/// Safe async subprocess wrapper around the host `adb` binary (never invokes a shell).
pub struct AdbInspector {
    telemetry: Arc<TelemetryStore>,
    adb_path: String,
    enabled: bool,
    preferred_serial: Option<String>,
    last_state_sig: Mutex<Option<Vec<(String, String)>>>,
}

impl AdbInspector {
    #[must_use]
    pub fn new(
        telemetry: Arc<TelemetryStore>,
        enabled: bool,
        preferred_serial: Option<String>,
    ) -> Self {
        Self {
            telemetry,
            adb_path: "adb".to_string(),
            enabled,
            preferred_serial,
            last_state_sig: Mutex::new(None),
        }
    }

    async fn run(&self, args: &[&str], timeout_secs: u64) -> Option<Vec<u8>> {
        if !self.enabled {
            return None;
        }
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd.spawn().ok()?;
        let out = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(out.stdout)
    }

    pub async fn list_devices(&self) -> Vec<AdbDeviceInfo> {
        let Some(raw) = self.run(&["devices", "-l"], 3).await else {
            return Vec::new();
        };
        let devices = parse_adb_devices(&String::from_utf8_lossy(&raw));
        let sig: Vec<(String, String)> =
            devices.iter().map(|d| (d.serial.clone(), d.state.clone())).collect();
        let changed = {
            let mut prev = self.last_state_sig.lock().expect("adb state mutex poisoned");
            if prev.as_ref() == Some(&sig) {
                false
            } else {
                *prev = Some(sig);
                true
            }
        };
        if changed {
            if devices.is_empty() {
                self.telemetry.log("info", "adb", "No ADB devices attached", None);
            } else {
                for d in &devices {
                    let lvl = if d.state == "device" { "ok" } else { "warn" };
                    self.telemetry.log(lvl, "adb", format!("ADB {}: {}", d.serial, d.state), None);
                }
            }
        }
        devices
    }

    #[must_use]
    pub fn select_authorized_serial(&self, devices: &[AdbDeviceInfo]) -> Option<String> {
        let authorized: Vec<&AdbDeviceInfo> =
            devices.iter().filter(|d| d.state == "device").collect();
        if authorized.is_empty() {
            return None;
        }
        if let Some(ref pref) = self.preferred_serial {
            if let Some(found) = authorized.iter().find(|d| &d.serial == pref) {
                return Some(found.serial.clone());
            }
        }
        Some(authorized[0].serial.clone())
    }

    async fn shell_text(&self, serial: &str, args: &[&str], timeout_secs: u64) -> String {
        let mut full = vec!["-s", serial, "shell"];
        full.extend_from_slice(args);
        match self.run(&full, timeout_secs).await {
            Some(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
            None => String::new(),
        }
    }

    pub async fn get_specs(&self, serial: &str) -> AndroidSpecs {
        let (props_raw, wm_sz, wm_dens, mem_raw, df_raw, bat_raw, kernel) = tokio::join!(
            self.shell_text(serial, &["getprop"], 4),
            self.shell_text(serial, &["wm", "size"], 3),
            self.shell_text(serial, &["wm", "density"], 3),
            self.shell_text(serial, &["cat", "/proc/meminfo"], 3),
            self.shell_text(serial, &["df", "-k", "/data"], 3),
            self.shell_text(serial, &["dumpsys", "battery"], 3),
            self.shell_text(serial, &["uname", "-r"], 3),
        );
        let props = parse_getprop(&props_raw);
        let density_dpi = parse_wm_density(&wm_dens);
        let display = parse_wm_size(&wm_sz).map(|(width, height)| DisplaySpecs {
            width,
            height,
            density_dpi,
        });
        AndroidSpecs {
            serial: serial.to_string(),
            manufacturer: props.get("ro.product.manufacturer").cloned().unwrap_or_default(),
            model: props.get("ro.product.model").cloned().unwrap_or_default(),
            device: props.get("ro.product.device").cloned().unwrap_or_default(),
            android_version: props.get("ro.build.version.release").cloned().unwrap_or_default(),
            sdk: props.get("ro.build.version.sdk").cloned().unwrap_or_default(),
            security_patch: props
                .get("ro.build.version.security_patch")
                .cloned()
                .unwrap_or_default(),
            hardware: props.get("ro.hardware").cloned().unwrap_or_default(),
            platform: props.get("ro.board.platform").cloned().unwrap_or_default(),
            abi: props.get("ro.product.cpu.abi").cloned().unwrap_or_default(),
            kernel,
            display,
            memory: parse_meminfo(&mem_raw),
            storage_data: parse_df_data(&df_raw),
            battery: parse_battery(&bat_raw),
        }
    }

    pub async fn capture_png(&self, serial: &str) -> Option<Vec<u8>> {
        let bytes = self.run(&["-s", serial, "exec-out", "screencap", "-p"], 5).await?;
        if !bytes.starts_with(&PNG_MAGIC) {
            return None;
        }
        self.telemetry.record_screen_frame();
        Some(bytes)
    }

    pub async fn tap(&self, serial: &str, x: i32, y: i32) -> Result<bool, String> {
        if !(0..=20_000).contains(&x) || !(0..=20_000).contains(&y) {
            return Err("Coordinates out of valid display range".to_string());
        }
        let xs = x.to_string();
        let ys = y.to_string();
        let ok = self.run(&["-s", serial, "shell", "input", "tap", &xs, &ys], 3).await.is_some();
        if ok {
            self.telemetry.record_adb_action();
            self.telemetry.log("info", "adb", format!("ADB tap ({x}, {y})"), None);
        }
        Ok(ok)
    }

    pub async fn swipe(
        &self,
        serial: &str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: i32,
    ) -> Result<bool, String> {
        for &c in &[x1, y1, x2, y2] {
            if !(0..=20_000).contains(&c) {
                return Err("Coordinates out of valid display range".to_string());
            }
        }
        let dur = duration_ms.clamp(50, 5000).to_string();
        let s1 = x1.to_string();
        let s2 = y1.to_string();
        let s3 = x2.to_string();
        let s4 = y2.to_string();
        let ok = self
            .run(&["-s", serial, "shell", "input", "swipe", &s1, &s2, &s3, &s4, &dur], 5)
            .await
            .is_some();
        if ok {
            self.telemetry.record_adb_action();
            self.telemetry.log(
                "info",
                "adb",
                format!("ADB swipe ({x1},{y1}) -> ({x2},{y2})"),
                None,
            );
        }
        Ok(ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices_list() {
        let out = "List of devices attached\nABC123\tdevice usb:1-2 product:test model:PhoneX device:phx\nDEF456\tunauthorized\n";
        let devs = parse_adb_devices(out);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].serial, "ABC123");
        assert_eq!(devs[0].state, "device");
        assert_eq!(devs[0].model, "PhoneX");
        assert_eq!(devs[1].state, "unauthorized");
    }

    #[test]
    fn parses_system_metrics() {
        let mem = parse_meminfo("MemTotal:        4096000 kB\nMemAvailable:    2048000 kB\n");
        assert_eq!(mem.total_mb, Some(4000));
        assert_eq!(mem.available_mb, Some(2000));

        let df = parse_df_data("Filesystem 1K-blocks Used Available Use% Mounted on\n/dev/block/dm-0 20971520 5242880 15728640 25% /data\n").unwrap();
        assert_eq!(df.total_gb, Some(20.0));
        assert_eq!(df.available_gb, Some(15.0));
        assert_eq!(df.use_pct, "25%");

        let bat = parse_battery(
            "AC powered: false\nUSB powered: true\nstatus: 2\nlevel: 84\ntemperature: 295\n",
        );
        assert_eq!(bat.level, Some(84));
        assert_eq!(bat.status.as_deref(), Some("Charging"));
        assert_eq!(bat.temperature_c, Some(29.5));
        assert_eq!(bat.powered.as_deref(), Some("USB"));

        assert_eq!(parse_wm_size("Physical size: 720x1600\n"), Some((720, 1600)));
        assert_eq!(parse_wm_density("Physical density: 320\n"), Some(320));
    }
}
