//! Google AOAv2 EP0 HID backend using pure-Rust `nusb` (zero `libusb` dependency).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nusb::descriptors::language_id::US_ENGLISH;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};
use nusb::{Device, DeviceInfo, MaybeFuture, Speed};
use serde::Serialize;

use crate::error::{AppError, Result};
use crate::hid::{
    ascii_to_hid, pack_keyboard_report, pack_mouse_report, shortcut_to_hid, KEYB_REPORT_DESC,
    MOD_SHIFT, MOUSE_REPORT_DESC,
};
use crate::telemetry::TelemetryStore;

const ACCESSORY_GET_PROTOCOL: u8 = 51;
const ACCESSORY_REGISTER_HID: u8 = 54;
const ACCESSORY_UNREGISTER_HID: u8 = 55;
const ACCESSORY_SET_HID_REPORT_DESC: u8 = 56;
const ACCESSORY_SEND_HID_EVENT: u8 = 57;

const HID_MOUSE_ID: u16 = 1;
const HID_KEYB_ID: u16 = 2;

const ROOT_HUB_VID: u16 = 0x1D6B;

#[derive(Clone, Debug, Serialize)]
pub struct UsbInterfaceSpec {
    pub number: u8,
    pub class_hex: String,
    pub class_name: String,
    pub subclass: u8,
    pub protocol: u8,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsbSpecs {
    pub vid: String,
    pub pid: String,
    pub product: String,
    pub manufacturer: String,
    pub serial: String,
    pub bus: String,
    pub address: u8,
    pub aoa_version: u16,
    pub usb_speed: String,
    pub usb_config: String,
    pub max_power_ma: Option<u16>,
    pub interfaces: Vec<UsbInterfaceSpec>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BridgeStatus {
    pub connected: bool,
    pub device: Option<UsbSpecs>,
    pub error: Option<String>,
}

/// Abstract USB OTG HID backend so server/routes can be integration-tested without hardware.
pub trait HidBackend: Send + Sync {
    fn status(&self, reconnect: bool) -> BridgeStatus;
    fn usb_specs(&self) -> Option<UsbSpecs>;
    fn send_mouse(
        &self,
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
        click: Option<&str>,
    ) -> Result<()>;
    fn swipe_up(&self) -> Result<()>;
    fn send_key(&self, mod_mask: u8, code: u8, shortcut: Option<&str>) -> Result<()>;
    fn send_text(&self, text: &str, press_enter: bool) -> Result<usize>;
    fn unlock_with_secret(
        &self,
        secret: Option<&str>,
        wake_method: &str,
        press_enter: bool,
        delay_ms: u64,
        use_preset: bool,
    ) -> Result<usize>;
    fn unlock_preset_configured(&self) -> bool;
    fn auto_unlock_enabled(&self) -> bool;
}

/// Cross-platform EP0 control transfer endpoint handle.
enum ControlHandle {
    #[cfg(not(target_os = "windows"))]
    Direct(Device),
    #[cfg(target_os = "windows")]
    Claimed(nusb::Interface),
}

impl ControlHandle {
    fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, String> {
        let req = ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value,
            index,
            length,
        };
        match self {
            #[cfg(not(target_os = "windows"))]
            Self::Direct(dev) => dev.control_in(req, timeout).wait().map_err(|e| e.to_string()),
            #[cfg(target_os = "windows")]
            Self::Claimed(intf) => intf.control_in(req, timeout).wait().map_err(|e| e.to_string()),
        }
    }

    fn control_out(
        &self,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        timeout: Duration,
    ) -> std::result::Result<(), String> {
        let req = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value,
            index,
            data,
        };
        match self {
            #[cfg(not(target_os = "windows"))]
            Self::Direct(dev) => dev.control_out(req, timeout).wait().map_err(|e| e.to_string()),
            #[cfg(target_os = "windows")]
            Self::Claimed(intf) => intf.control_out(req, timeout).wait().map_err(|e| e.to_string()),
        }
    }
}

struct ConnectedDevice {
    device: Device,
    ctrl: ControlHandle,
    info: UsbSpecs,
}

struct BridgeInner {
    connected: Option<ConnectedDevice>,
    last_error: Option<String>,
    buttons: u8,
    last_mouse_log: Instant,
    last_probe_err: Option<String>,
    auto_unlocked_keys: HashSet<String>,
}

/// Filters used to target or exclude specific USB devices.
#[derive(Clone, Debug, Default)]
pub struct DeviceFilter {
    pub serial: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub ignore_vids: HashSet<u16>,
}

/// Real USB bridge implementing Google AOAv2 HID via `nusb`.
pub struct NusbAoaBridge {
    telemetry: Arc<TelemetryStore>,
    filter: DeviceFilter,
    unlock_preset: Option<String>,
    auto_unlock: bool,
    auto_unlock_wake: String,
    inner: Mutex<BridgeInner>,
}

impl NusbAoaBridge {
    #[must_use]
    pub fn new(
        telemetry: Arc<TelemetryStore>,
        filter: DeviceFilter,
        unlock_preset: Option<String>,
        auto_unlock: bool,
        auto_unlock_wake: String,
    ) -> Self {
        Self {
            telemetry,
            filter,
            unlock_preset: unlock_preset.filter(|s| !s.is_empty()),
            auto_unlock,
            auto_unlock_wake,
            inner: Mutex::new(BridgeInner {
                connected: None,
                last_error: None,
                buttons: 0,
                last_mouse_log: Instant::now() - Duration::from_secs(10),
                last_probe_err: None,
                auto_unlocked_keys: HashSet::new(),
            }),
        }
    }

    fn usb_class_name(cls: u8) -> &'static str {
        match cls {
            0x00 => "Per-Interface",
            0x01 => "Audio",
            0x02 => "CDC",
            0x03 => "HID",
            0x06 => "Imaging (MTP/PTP)",
            0x08 => "Mass Storage",
            0x09 => "Hub",
            0x0A => "CDC-Data",
            0x0E => "Video",
            0xE0 => "Wireless",
            0xEF => "Misc",
            0xFE => "App Specific",
            0xFF => "Vendor Specific (ADB/Fastboot)",
            _ => "Other",
        }
    }

    fn speed_label(speed: Option<Speed>) -> String {
        match speed {
            Some(Speed::Low) => "Low Speed (1.5 Mbps)".to_string(),
            Some(Speed::Full) => "Full Speed (12 Mbps)".to_string(),
            Some(Speed::High) => "High Speed (480 Mbps)".to_string(),
            Some(Speed::Super) => "SuperSpeed (5 Gbps)".to_string(),
            Some(Speed::SuperPlus) => "SuperSpeed+ (10 Gbps)".to_string(),
            _ => "Unknown".to_string(),
        }
    }

    fn fetch_str(dev: &Device, idx: Option<std::num::NonZeroU8>) -> String {
        let Some(index) = idx else {
            return String::new();
        };
        dev.get_string_descriptor(index, US_ENGLISH, Duration::from_millis(500))
            .wait()
            .unwrap_or_default()
    }

    fn read_specs(info: &DeviceInfo, dev: &Device, aoa_version: u16) -> UsbSpecs {
        let desc = dev.device_descriptor();
        let product = info
            .product_string()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::fetch_str(dev, desc.product_string_index()));
        let manufacturer = info
            .manufacturer_string()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::fetch_str(dev, desc.manufacturer_string_index()));
        let serial = info
            .serial_number()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Self::fetch_str(dev, desc.serial_number_string_index()));

        let mut usb_config = String::from("unknown");
        let mut max_power_ma = None;
        let mut interfaces = Vec::new();

        if let Ok(cfg) = dev.active_configuration() {
            let cfg_name = Self::fetch_str(dev, cfg.string_index());
            if !cfg_name.is_empty() {
                usb_config = cfg_name;
            }
            max_power_ma = Some(u16::from(cfg.max_power()) * 2);
            for grp in cfg.interfaces() {
                let alt = grp.first_alt_setting();
                let cls = alt.class();
                interfaces.push(UsbInterfaceSpec {
                    number: alt.interface_number(),
                    class_hex: format!("0x{cls:02x}"),
                    class_name: Self::usb_class_name(cls).to_string(),
                    subclass: alt.subclass(),
                    protocol: alt.protocol(),
                    name: Self::fetch_str(dev, alt.string_index()),
                });
            }
        }

        UsbSpecs {
            vid: format!("{:04x}", info.vendor_id()),
            pid: format!("{:04x}", info.product_id()),
            product: if product.is_empty() { "Android Device".to_string() } else { product },
            manufacturer,
            serial,
            bus: info.bus_id().to_string(),
            address: info.device_address(),
            aoa_version,
            usb_speed: Self::speed_label(info.speed()),
            usb_config,
            max_power_ma,
            interfaces,
        }
    }

    fn open_control(dev: &Device) -> std::result::Result<ControlHandle, String> {
        #[cfg(not(target_os = "windows"))]
        {
            Ok(ControlHandle::Direct(dev.clone()))
        }
        #[cfg(target_os = "windows")]
        {
            let preferred_intf = dev
                .active_configuration()
                .ok()
                .and_then(|cfg| {
                    let all: Vec<_> = cfg.interfaces().map(|g| g.first_alt_setting()).collect();
                    all.iter()
                        .find(|a| a.class() != 0xFF)
                        .or_else(|| all.first())
                        .map(|a| a.interface_number())
                })
                .unwrap_or(0);
            let intf = dev
                .claim_interface(preferred_intf)
                .wait()
                .map_err(|e| format!("claim interface {preferred_intf}: {e}"))?;
            Ok(ControlHandle::Claimed(intf))
        }
    }

    fn register_hid(
        ctrl: &ControlHandle,
        hid_id: u16,
        desc: &[u8],
        max_pkt0: u8,
    ) -> std::result::Result<(), String> {
        let timeout = Duration::from_millis(1000);
        let _ =
            ctrl.control_out(ACCESSORY_UNREGISTER_HID, hid_id, 0, &[], Duration::from_millis(400));
        ctrl.control_out(ACCESSORY_REGISTER_HID, hid_id, desc.len() as u16, &[], timeout)?;
        let chunk_size = (max_pkt0.max(8) as usize).min(63);
        let mut offset = 0usize;
        while offset < desc.len() {
            let end = (offset + chunk_size).min(desc.len());
            ctrl.control_out(
                ACCESSORY_SET_HID_REPORT_DESC,
                hid_id,
                offset as u16,
                &desc[offset..end],
                timeout,
            )?;
            offset = end;
        }
        Ok(())
    }

    fn matches_filter(&self, dev: &DeviceInfo) -> bool {
        let vid = dev.vendor_id();
        let pid = dev.product_id();
        if vid == ROOT_HUB_VID || self.filter.ignore_vids.contains(&vid) || dev.class() == 0x09 {
            return false;
        }
        if self.filter.vid.is_some_and(|v| v != vid) {
            return false;
        }
        if self.filter.pid.is_some_and(|p| p != pid) {
            return false;
        }
        if let Some(ref target_serial) = self.filter.serial {
            if let Some(cached) = dev.serial_number() {
                if cached != target_serial {
                    return false;
                }
            } else if let Ok(Some(fetched)) = dev.get_serial_number_string().wait() {
                if &fetched != target_serial {
                    return false;
                }
            }
        }
        true
    }

    fn disconnect_locked(st: &mut BridgeInner) {
        if let Some(conn) = st.connected.take() {
            for hid_id in [HID_MOUSE_ID, HID_KEYB_ID] {
                let _ = conn.ctrl.control_out(
                    ACCESSORY_UNREGISTER_HID,
                    hid_id,
                    0,
                    &[],
                    Duration::from_millis(200),
                );
            }
        }
        st.buttons = 0;
    }

    fn connect_locked(&self, st: &mut BridgeInner, force: bool) -> bool {
        if st.connected.is_some() && !force {
            return true;
        }
        if force {
            Self::disconnect_locked(st);
        }

        let listed: Vec<DeviceInfo> = match nusb::list_devices().wait() {
            Ok(iter) => iter.filter(|d| self.matches_filter(d)).collect(),
            Err(err) => {
                st.last_error = Some(format!("USB enumeration failed: {err}"));
                return false;
            }
        };

        if listed.is_empty() {
            st.auto_unlocked_keys.clear();
            let msg = "No Android USB device detected".to_string();
            if st.last_probe_err.as_deref() != Some(msg.as_str()) {
                self.telemetry.log("info", "usb", msg.clone(), None);
                st.last_probe_err = Some(msg.clone());
            }
            st.last_error = Some(msg);
            return false;
        }

        let mut last_err = None;
        for dev_info in listed {
            let vid = dev_info.vendor_id();
            let pid = dev_info.product_id();
            let dev = match dev_info.open().wait() {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(format!("USB {vid:04x}:{pid:04x}: {e}"));
                    continue;
                }
            };
            let ctrl = match Self::open_control(&dev) {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(format!("USB {vid:04x}:{pid:04x}: {e}"));
                    continue;
                }
            };
            let raw =
                match ctrl.control_in(ACCESSORY_GET_PROTOCOL, 0, 0, 2, Duration::from_millis(1000))
                {
                    Ok(r) => r,
                    Err(e) => {
                        last_err = Some(format!("USB {vid:04x}:{pid:04x}: {e}"));
                        continue;
                    }
                };
            if raw.len() < 2 {
                continue;
            }
            let aoa_ver = u16::from_le_bytes([raw[0], raw[1]]);
            if aoa_ver < 2 {
                last_err = Some(format!("USB {vid:04x}:{pid:04x} reported AOA v{aoa_ver} (< 2)"));
                continue;
            }
            let max_pkt0 = dev.device_descriptor().max_packet_size_0();
            if let Err(e) = Self::register_hid(&ctrl, HID_MOUSE_ID, &MOUSE_REPORT_DESC, max_pkt0) {
                last_err = Some(format!("USB {vid:04x}:{pid:04x} mouse register: {e}"));
                continue;
            }
            if let Err(e) = Self::register_hid(&ctrl, HID_KEYB_ID, &KEYB_REPORT_DESC, max_pkt0) {
                last_err = Some(format!("USB {vid:04x}:{pid:04x} keyboard register: {e}"));
                continue;
            }

            let specs = Self::read_specs(&dev_info, &dev, aoa_ver);
            self.telemetry.log(
                "ok",
                "aoa",
                format!(
                    "AOAv{} HID attached: {} [{}:{}]",
                    aoa_ver, specs.product, specs.vid, specs.pid
                ),
                Some(serde_json::json!({ "serial": specs.serial, "usb_config": specs.usb_config })),
            );
            let dev_key = format!(
                "{}:{}",
                specs.vid,
                if specs.serial.is_empty() { specs.bus.clone() } else { specs.serial.clone() }
            );
            st.connected = Some(ConnectedDevice { device: dev, ctrl, info: specs });
            st.buttons = 0;
            st.last_error = None;
            st.last_probe_err = None;

            if self.auto_unlock && !st.auto_unlocked_keys.contains(&dev_key) {
                if let Some(ref preset) = self.unlock_preset {
                    st.auto_unlocked_keys.insert(dev_key);
                    if let Err(err) =
                        self.unlock_locked(st, preset, &self.auto_unlock_wake, true, 600, true)
                    {
                        self.telemetry.log(
                            "error",
                            "hid",
                            format!("Auto-unlock failed: {err}"),
                            None,
                        );
                    }
                }
            }
            return true;
        }

        let err_msg = last_err.unwrap_or_else(|| "No AOAv2-capable USB device found".to_string());
        if st.last_probe_err.as_deref() != Some(err_msg.as_str()) {
            self.telemetry.log("warn", "usb", err_msg.clone(), None);
            st.last_probe_err = Some(err_msg.clone());
        }
        st.last_error = Some(err_msg);
        false
    }

    fn send_hid_locked(
        &self,
        st: &mut BridgeInner,
        hid_id: u16,
        report: &[u8],
        kind: &str,
    ) -> Result<()> {
        if !self.connect_locked(st, false) {
            return Err(AppError::Usb(
                st.last_error.clone().unwrap_or_else(|| "No AOAv2 device connected".to_string()),
            ));
        }
        let t0 = Instant::now();
        let send_res = st.connected.as_ref().expect("connected checked").ctrl.control_out(
            ACCESSORY_SEND_HID_EVENT,
            hid_id,
            0,
            report,
            Duration::from_millis(1000),
        );

        if let Err(first_err) = send_res {
            self.telemetry.log(
                "warn",
                "usb",
                format!("USB handle reset ({first_err}), re-attaching"),
                None,
            );
            if !self.connect_locked(st, true) {
                return Err(AppError::Usb(
                    st.last_error
                        .clone()
                        .unwrap_or_else(|| "USB re-enumeration failed".to_string()),
                ));
            }
            let t_retry = Instant::now();
            st.connected
                .as_ref()
                .expect("reconnected")
                .ctrl
                .control_out(
                    ACCESSORY_SEND_HID_EVENT,
                    hid_id,
                    0,
                    report,
                    Duration::from_millis(1000),
                )
                .map_err(AppError::Usb)?;
            self.telemetry.record_hid(kind, t_retry.elapsed().as_secs_f64() * 1000.0);
            return Ok(());
        }
        self.telemetry.record_hid(kind, t0.elapsed().as_secs_f64() * 1000.0);
        Ok(())
    }

    fn swipe_up_locked(&self, st: &mut BridgeInner) -> Result<()> {
        self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(MOD_SHIFT, 0), "key")?;
        thread::sleep(Duration::from_millis(30));
        self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0), "key")?;
        thread::sleep(Duration::from_millis(80));
        for _ in 0..5 {
            self.send_hid_locked(st, HID_MOUSE_ID, &pack_mouse_report(0, 0, 80, 0, 0), "mouse")?;
            thread::sleep(Duration::from_millis(10));
        }
        self.send_hid_locked(st, HID_MOUSE_ID, &pack_mouse_report(1, 0, 0, 0, 0), "mouse")?;
        thread::sleep(Duration::from_millis(30));
        for _ in 0..12 {
            self.send_hid_locked(st, HID_MOUSE_ID, &pack_mouse_report(1, 0, -40, 0, 0), "mouse")?;
            thread::sleep(Duration::from_millis(15));
        }
        self.send_hid_locked(st, HID_MOUSE_ID, &pack_mouse_report(0, 0, 0, 0, 0), "mouse")?;
        st.buttons = 0;
        Ok(())
    }

    fn type_ascii_locked(
        &self,
        st: &mut BridgeInner,
        text: &str,
        press_enter: bool,
        cadence_ms: u64,
    ) -> Result<usize> {
        let mut sent = 0usize;
        for ch in text.chars() {
            let Some((mod_mask, code)) = ascii_to_hid(ch) else {
                continue;
            };
            self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(mod_mask, code), "key")?;
            thread::sleep(Duration::from_millis(cadence_ms));
            self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0), "key")?;
            thread::sleep(Duration::from_millis(cadence_ms));
            sent += 1;
        }
        if press_enter {
            self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0x28), "key")?;
            thread::sleep(Duration::from_millis(20));
            self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0), "key")?;
        }
        Ok(sent)
    }

    fn unlock_locked(
        &self,
        st: &mut BridgeInner,
        secret: &str,
        wake_method: &str,
        press_enter: bool,
        delay_ms: u64,
        preset: bool,
    ) -> Result<usize> {
        if secret.is_empty() || secret.len() > 128 {
            return Err(AppError::BadRequest("Unlock secret must be 1..128 characters".into()));
        }
        if !matches!(wake_method, "swipe" | "space" | "none") {
            return Err(AppError::BadRequest(
                "wake_method must be 'swipe', 'space', or 'none'".into(),
            ));
        }
        let clamped_delay = delay_ms.clamp(150, 3000);
        match wake_method {
            "swipe" => {
                self.swipe_up_locked(st)?;
                thread::sleep(Duration::from_millis(clamped_delay));
            }
            "space" => {
                for _ in 0..2 {
                    self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0x2C), "key")?;
                    thread::sleep(Duration::from_millis(30));
                    self.send_hid_locked(st, HID_KEYB_ID, &pack_keyboard_report(0, 0), "key")?;
                    thread::sleep(Duration::from_millis(550));
                }
                thread::sleep(Duration::from_millis(clamped_delay));
            }
            _ => {}
        }
        let sent = self.type_ascii_locked(st, secret, press_enter, 20)?;
        // Never log or echo the secret string itself
        self.telemetry.log(
            "info",
            "hid",
            format!("Unlock sequence sent ({sent} chars, wake={wake_method}, enter={press_enter}, preset={preset})"),
            None,
        );
        Ok(sent)
    }
}

impl HidBackend for NusbAoaBridge {
    fn status(&self, reconnect: bool) -> BridgeStatus {
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        if reconnect {
            self.telemetry.record_reconnect();
            self.telemetry.log("info", "usb", "Manual USB OTG reconnect triggered", None);
            self.connect_locked(&mut st, true);
        } else if let Some(ref conn) = st.connected {
            let alive = conn
                .ctrl
                .control_in(ACCESSORY_GET_PROTOCOL, 0, 0, 2, Duration::from_millis(500))
                .is_ok();
            let _ = conn.device.device_descriptor();
            if !alive {
                self.telemetry.log("warn", "usb", "USB connection lost, re-probing", None);
                self.connect_locked(&mut st, true);
            }
        } else {
            self.connect_locked(&mut st, false);
        }

        BridgeStatus {
            connected: st.connected.is_some(),
            device: st.connected.as_ref().map(|c| c.info.clone()),
            error: st.last_error.clone(),
        }
    }

    fn usb_specs(&self) -> Option<UsbSpecs> {
        let st = self.inner.lock().expect("bridge mutex poisoned");
        st.connected.as_ref().map(|c| c.info.clone())
    }

    fn send_mouse(
        &self,
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
        click: Option<&str>,
    ) -> Result<()> {
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        if let Some(btn) = click {
            let bit = match btn {
                "left" => 1,
                "right" => 2,
                "middle" => 4,
                _ => {
                    return Err(AppError::BadRequest(format!("Unknown mouse click button: {btn}")))
                }
            };
            self.send_hid_locked(
                &mut st,
                HID_MOUSE_ID,
                &pack_mouse_report(bit, 0, 0, 0, 0),
                "mouse",
            )?;
            thread::sleep(Duration::from_millis(30));
            self.send_hid_locked(
                &mut st,
                HID_MOUSE_ID,
                &pack_mouse_report(0, 0, 0, 0, 0),
                "mouse",
            )?;
            st.buttons = 0;
            self.telemetry.log("info", "hid", format!("Mouse {btn} click"), None);
            return Ok(());
        }
        if let Some(b) = buttons {
            st.buttons = b & 0x1F;
        }
        let report = pack_mouse_report(st.buttons, dx, dy, vscroll, hscroll);
        self.send_hid_locked(&mut st, HID_MOUSE_ID, &report, "mouse")?;
        if st.last_mouse_log.elapsed() >= Duration::from_secs(2) {
            st.last_mouse_log = Instant::now();
            self.telemetry.log(
                "info",
                "hid",
                "Mouse HID stream active",
                Some(serde_json::json!({ "buttons": st.buttons, "dx": dx, "dy": dy })),
            );
        }
        Ok(())
    }

    fn swipe_up(&self) -> Result<()> {
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        self.swipe_up_locked(&mut st)?;
        self.telemetry.log("info", "hid", "Macro executed: Swipe Up (unlock)", None);
        Ok(())
    }

    fn send_key(&self, mut mod_mask: u8, mut code: u8, shortcut: Option<&str>) -> Result<()> {
        if let Some(name) = shortcut {
            let (m, c) = shortcut_to_hid(name)
                .ok_or_else(|| AppError::BadRequest(format!("Unknown shortcut: {name}")))?;
            mod_mask = m;
            code = c;
        }
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        self.send_hid_locked(&mut st, HID_KEYB_ID, &pack_keyboard_report(mod_mask, code), "key")?;
        thread::sleep(Duration::from_millis(30));
        self.send_hid_locked(&mut st, HID_KEYB_ID, &pack_keyboard_report(0, 0), "key")?;
        let label = shortcut
            .map(str::to_string)
            .unwrap_or_else(|| format!("mod=0x{mod_mask:02x} code=0x{code:02x}"));
        self.telemetry.log("info", "hid", format!("Key event: {label}"), None);
        Ok(())
    }

    fn send_text(&self, text: &str, press_enter: bool) -> Result<usize> {
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        let sent = self.type_ascii_locked(&mut st, text, press_enter, 15)?;
        // Log character count only (never log typed text/passwords)
        self.telemetry.log(
            "info",
            "hid",
            format!("Typed text ({sent} chars, enter={press_enter})"),
            None,
        );
        Ok(sent)
    }

    fn unlock_with_secret(
        &self,
        secret: Option<&str>,
        wake_method: &str,
        press_enter: bool,
        delay_ms: u64,
        use_preset: bool,
    ) -> Result<usize> {
        let resolved = if use_preset {
            self.unlock_preset
                .as_deref()
                .ok_or_else(|| AppError::BadRequest("No unlock preset configured".into()))?
        } else {
            secret.ok_or_else(|| AppError::BadRequest("secret must be 1..128 characters".into()))?
        };
        let mut st = self.inner.lock().expect("bridge mutex poisoned");
        self.unlock_locked(&mut st, resolved, wake_method, press_enter, delay_ms, use_preset)
    }

    fn unlock_preset_configured(&self) -> bool {
        self.unlock_preset.is_some()
    }

    fn auto_unlock_enabled(&self) -> bool {
        self.auto_unlock
    }
}

/// Spawn background `nusb::watch_devices()` task to probe AOAv2 immediately on USB hotplug.
pub fn spawn_hotplug_watcher(bridge: Arc<dyn HidBackend>, telemetry: Arc<TelemetryStore>) {
    let Ok(mut watch) = nusb::watch_devices() else {
        telemetry.log("warn", "usb", "USB hotplug watch unavailable; using active polling", None);
        return;
    };
    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(ev) = watch.next().await {
            match ev {
                nusb::hotplug::HotplugEvent::Connected(dev) => {
                    if dev.vendor_id() != ROOT_HUB_VID {
                        telemetry.log(
                            "info",
                            "usb",
                            format!(
                                "USB hotplug connected {:04x}:{:04x}",
                                dev.vendor_id(),
                                dev.product_id()
                            ),
                            None,
                        );
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let b = bridge.clone();
                        let _ = tokio::task::spawn_blocking(move || b.status(false)).await;
                    }
                }
                nusb::hotplug::HotplugEvent::Disconnected(_) => {
                    let b = bridge.clone();
                    let _ = tokio::task::spawn_blocking(move || b.status(false)).await;
                }
            }
        }
    });
}
