//! Google AOAv2 EP0 HID backend powered by an async single-owner `nusb` actor.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use nusb::descriptors::language_id::US_ENGLISH;
use nusb::hotplug::HotplugEvent;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, TransferError};
use nusb::{Device, DeviceId, DeviceInfo, Speed};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use crate::error::{AppError, Result};
use crate::hid::{
    ascii_to_hid, normalize_newlines, pack_keyboard_report, pack_mouse_report, shortcut_to_hid,
    validate_ascii_secret, KEYB_REPORT_DESC, MOD_SHIFT, MOUSE_REPORT_DESC,
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
const MAX_TEXT_CHARS: usize = 512;
const MAX_UNLOCK_CHARS: usize = 128;
const HID_SETTLE_DELAY: Duration = Duration::from_millis(200);
const STALL_BACKOFF_MS: [u64; 3] = [40, 80, 160];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbError {
    Stall,
    Disconnected,
    Timeout,
    Other(String),
}

impl fmt::Display for UsbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stall => write!(f, "endpoint stalled"),
            Self::Disconnected => write!(f, "device disconnected"),
            Self::Timeout => write!(f, "USB transfer timed out"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl From<TransferError> for UsbError {
    fn from(err: TransferError) -> Self {
        match err {
            TransferError::Stall => Self::Stall,
            TransferError::Disconnected => Self::Disconnected,
            TransferError::Cancelled => Self::Timeout,
            other => Self::Other(other.to_string()),
        }
    }
}

/// Low-level vendor control transport (abstracted so AOAv2 state machine can be unit-tested).
pub trait UsbTransport: Send + Sync {
    fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, UsbError>>;

    fn control_out<'a>(
        &'a self,
        request: u8,
        value: u16,
        index: u16,
        data: &'a [u8],
        timeout: Duration,
    ) -> BoxFuture<'a, std::result::Result<(), UsbError>>;
}

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

/// Async USB OTG HID backend interface for server handlers and integration tests.
pub trait HidBackend: Send + Sync {
    fn status_snapshot(&self) -> BridgeStatus;
    fn usb_specs(&self) -> Option<UsbSpecs>;
    fn reconnect(&self) -> BoxFuture<'_, BridgeStatus>;
    fn send_mouse(
        &self,
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
        click: Option<String>,
    ) -> BoxFuture<'_, Result<()>>;
    fn swipe_up(&self) -> BoxFuture<'_, Result<()>>;
    fn send_key(
        &self,
        mod_mask: u8,
        code: u8,
        shortcut: Option<String>,
    ) -> BoxFuture<'_, Result<()>>;
    fn send_text(&self, text: String, press_enter: bool) -> BoxFuture<'_, Result<usize>>;
    fn unlock_with_secret(
        &self,
        secret: Option<Zeroizing<String>>,
        wake_method: String,
        press_enter: bool,
        delay_ms: u64,
        use_preset: bool,
    ) -> BoxFuture<'_, Result<usize>>;
    fn unlock_preset_configured(&self) -> bool;
    fn auto_unlock_enabled(&self) -> bool;
}

/// Filters used to target or exclude specific USB devices.
#[derive(Clone, Debug, Default)]
pub struct DeviceFilter {
    pub serial: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub ignore_vids: HashSet<u16>,
}

enum ControlHandle {
    #[cfg(not(target_os = "windows"))]
    Direct(Device),
    #[cfg(target_os = "windows")]
    Claimed(nusb::Interface),
}

impl UsbTransport for ControlHandle {
    fn control_in(
        &self,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: Duration,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, UsbError>> {
        Box::pin(async move {
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
                Self::Direct(dev) => dev.control_in(req, timeout).await.map_err(UsbError::from),
                #[cfg(target_os = "windows")]
                Self::Claimed(intf) => intf.control_in(req, timeout).await.map_err(UsbError::from),
            }
        })
    }

    fn control_out<'a>(
        &'a self,
        request: u8,
        value: u16,
        index: u16,
        data: &'a [u8],
        timeout: Duration,
    ) -> BoxFuture<'a, std::result::Result<(), UsbError>> {
        Box::pin(async move {
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
                Self::Direct(dev) => dev.control_out(req, timeout).await.map_err(UsbError::from),
                #[cfg(target_os = "windows")]
                Self::Claimed(intf) => intf.control_out(req, timeout).await.map_err(UsbError::from),
            }
        })
    }
}

/// Formats OS-specific permission or USB driver remediation hints on open/claim failures.
pub(crate) fn format_usb_open_hint(os: &str, vid: u16, pid: u16, raw_err: &str) -> String {
    let lower = raw_err.to_ascii_lowercase();
    match os {
        "linux"
            if lower.contains("permission")
                || lower.contains("denied")
                || lower.contains("eacces") =>
        {
            format!(
                "USB {vid:04x}:{pid:04x}: permission denied — install deploy/51-android.rules (udev) and join plugdev (or use console uaccess): {raw_err}"
            )
        }
        "windows"
            if lower.contains("winusb")
                || lower.contains("access")
                || lower.contains("denied")
                || lower.contains("claim")
                || lower.contains("not supported") =>
        {
            format!(
                "USB {vid:04x}:{pid:04x}: Windows inbox MTP driver blocked EP0 — bind WinUSB via Zadig or attach via WSL2 + usbipd-win ({raw_err})"
            )
        }
        "macos"
            if lower.contains("exclusive")
                || lower.contains("access")
                || lower.contains("busy")
                || lower.contains("privilege") =>
        {
            format!(
                "USB {vid:04x}:{pid:04x}: device busy or blocked on macOS — close Android File Transfer / OpenMTP / adb and retry ({raw_err})"
            )
        }
        _ => format!("USB {vid:04x}:{pid:04x}: {raw_err}"),
    }
}

/// Compute safe EP0 chunk size (`<= 63` bytes) honoring USB 3.0 exponent encoding of `bMaxPacketSize0`.
#[must_use]
pub fn aoa_hid_chunk_size(usb_version: u16, max_pkt0: u8) -> usize {
    let ep0 = if usb_version >= 0x0300 {
        1usize.checked_shl(u32::from(max_pkt0.min(10))).unwrap_or(512)
    } else {
        usize::from(max_pkt0).max(8)
    };
    ep0.clamp(8, 63)
}

/// Decode `bMaxPower` into milliamps using USB 2.0 (2 mA/unit) or USB 3.0 (8 mA/unit) scaling.
#[must_use]
pub fn usb_max_power_ma(usb_version: u16, raw_max_power: u8) -> u16 {
    let unit = if usb_version >= 0x0300 { 8 } else { 2 };
    u16::from(raw_max_power).saturating_mul(unit)
}

async fn register_hid_desc(
    transport: &dyn UsbTransport,
    hid_id: u16,
    desc: &[u8],
    chunk_size: usize,
) -> std::result::Result<(), UsbError> {
    let timeout = Duration::from_millis(1000);
    let _ = transport
        .control_out(ACCESSORY_UNREGISTER_HID, hid_id, 0, &[], Duration::from_millis(300))
        .await;
    transport.control_out(ACCESSORY_REGISTER_HID, hid_id, desc.len() as u16, &[], timeout).await?;
    let mut offset = 0usize;
    while offset < desc.len() {
        let end = (offset + chunk_size).min(desc.len());
        transport
            .control_out(
                ACCESSORY_SET_HID_REPORT_DESC,
                hid_id,
                offset as u16,
                &desc[offset..end],
                timeout,
            )
            .await?;
        offset = end;
    }
    Ok(())
}

/// Register mouse (`id=1`) and keyboard (`id=2`), rolling back mouse if keyboard fails.
pub async fn register_hid_pair(
    transport: &dyn UsbTransport,
    usb_version: u16,
    max_pkt0: u8,
) -> std::result::Result<(), UsbError> {
    let chunk = aoa_hid_chunk_size(usb_version, max_pkt0);
    register_hid_desc(transport, HID_MOUSE_ID, &MOUSE_REPORT_DESC, chunk).await?;
    if let Err(err) = register_hid_desc(transport, HID_KEYB_ID, &KEYB_REPORT_DESC, chunk).await {
        let _ = transport
            .control_out(ACCESSORY_UNREGISTER_HID, HID_MOUSE_ID, 0, &[], Duration::from_millis(250))
            .await;
        return Err(err);
    }
    Ok(())
}

async fn sleep_or_shutdown(shutdown_rx: &mut watch::Receiver<bool>, dur: Duration) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        res = shutdown_rx.changed() => res.is_err() || *shutdown_rx.borrow(),
    }
}

/// Send one HID input report, retrying transient `EP0` stalls caused by async kernel `hid_add_device`.
pub async fn send_hid_with_stall_retry(
    transport: &dyn UsbTransport,
    hid_id: u16,
    report: &[u8],
    shutdown_rx: &mut watch::Receiver<bool>,
) -> std::result::Result<(), UsbError> {
    let timeout = Duration::from_millis(1000);
    let mut attempt = 0usize;
    loop {
        match transport.control_out(ACCESSORY_SEND_HID_EVENT, hid_id, 0, report, timeout).await {
            Ok(()) => return Ok(()),
            Err(UsbError::Stall) if attempt < STALL_BACKOFF_MS.len() => {
                let wait = Duration::from_millis(STALL_BACKOFF_MS[attempt]);
                attempt += 1;
                if sleep_or_shutdown(shutdown_rx, wait).await {
                    return Err(UsbError::Other("shutdown requested".into()));
                }
            }
            Err(err) => return Err(err),
        }
    }
}

/// Best-effort release zero reports and `ACCESSORY_UNREGISTER_HID` cleanup.
pub async fn release_and_unregister_hid(transport: &dyn UsbTransport) {
    let timeout = Duration::from_millis(250);
    let _ =
        transport.control_out(ACCESSORY_SEND_HID_EVENT, HID_MOUSE_ID, 0, &[0; 5], timeout).await;
    let _ = transport.control_out(ACCESSORY_SEND_HID_EVENT, HID_KEYB_ID, 0, &[0; 8], timeout).await;
    let _ = transport.control_out(ACCESSORY_UNREGISTER_HID, HID_MOUSE_ID, 0, &[], timeout).await;
    let _ = transport.control_out(ACCESSORY_UNREGISTER_HID, HID_KEYB_ID, 0, &[], timeout).await;
}

#[derive(Default)]
struct PendingMouse {
    dx: i32,
    dy: i32,
    vscroll: i32,
    hscroll: i32,
    buttons: Option<u8>,
    dirty: bool,
}

enum ActorCmd {
    Reconnect(oneshot::Sender<BridgeStatus>),
    MouseMove {
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    MouseClick {
        button: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SwipeUp {
        reply: oneshot::Sender<Result<()>>,
    },
    Key {
        mod_mask: u8,
        code: u8,
        shortcut: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    Text {
        text: String,
        press_enter: bool,
        reply: oneshot::Sender<Result<usize>>,
    },
    Unlock {
        secret: Option<Zeroizing<String>>,
        wake_method: String,
        press_enter: bool,
        delay_ms: u64,
        use_preset: bool,
        reply: oneshot::Sender<Result<usize>>,
    },
}

struct ConnectedDevice {
    device_id: DeviceId,
    dev_key: String,
    transport: Arc<dyn UsbTransport>,
    info: UsbSpecs,
}

/// Public clone-cheap handle to the async AOAv2 USB actor.
pub struct NusbAoaBridge {
    cmd_tx: mpsc::Sender<ActorCmd>,
    pending_mouse: Arc<Mutex<PendingMouse>>,
    mouse_notify: Arc<Notify>,
    status_rx: watch::Receiver<BridgeStatus>,
    has_preset: bool,
    auto_unlock: bool,
}

impl NusbAoaBridge {
    /// Spawn the single-owner AOAv2 USB actor and return `(bridge_handle, join_handle)`.
    #[must_use]
    pub fn spawn(
        telemetry: Arc<TelemetryStore>,
        filter: DeviceFilter,
        unlock_preset: Option<Zeroizing<String>>,
        auto_unlock: bool,
        auto_unlock_wake: String,
        shutdown_rx: watch::Receiver<bool>,
    ) -> (Arc<Self>, JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let initial = BridgeStatus {
            connected: false,
            device: None,
            error: Some("Initializing USB OTG probe...".into()),
        };
        let (status_tx, status_rx) = watch::channel(initial);
        let pending_mouse = Arc::new(Mutex::new(PendingMouse::default()));
        let mouse_notify = Arc::new(Notify::new());
        let has_preset = unlock_preset.as_ref().is_some_and(|s| !s.is_empty());

        let actor = AoaActor {
            telemetry,
            filter,
            unlock_preset: unlock_preset.filter(|s| !s.is_empty()),
            auto_unlock,
            auto_unlock_wake,
            cmd_rx,
            pending_mouse: pending_mouse.clone(),
            mouse_notify: mouse_notify.clone(),
            status_tx,
            shutdown_rx,
            connected: None,
            buttons: 0,
            last_error: None,
            last_probe_err: None,
            last_mouse_log: Instant::now() - Duration::from_secs(10),
            auto_unlocked_keys: HashSet::new(),
        };

        let handle = tokio::spawn(actor.run());
        let bridge = Arc::new(Self {
            cmd_tx,
            pending_mouse,
            mouse_notify,
            status_rx,
            has_preset,
            auto_unlock,
        });
        (bridge, handle)
    }

    async fn call_cmd<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> ActorCmd,
    ) -> std::result::Result<T, AppError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(build(tx))
            .await
            .map_err(|_| AppError::ServiceUnavailable("USB bridge actor stopped".into()))?;
        rx.await.map_err(|_| AppError::ServiceUnavailable("USB bridge request cancelled".into()))
    }
}

impl HidBackend for NusbAoaBridge {
    fn status_snapshot(&self) -> BridgeStatus {
        self.status_rx.borrow().clone()
    }

    fn usb_specs(&self) -> Option<UsbSpecs> {
        self.status_rx.borrow().device.clone()
    }

    fn reconnect(&self) -> BoxFuture<'_, BridgeStatus> {
        Box::pin(async move {
            match self.call_cmd(ActorCmd::Reconnect).await {
                Ok(st) => st,
                Err(err) => {
                    BridgeStatus { connected: false, device: None, error: Some(err.to_string()) }
                }
            }
        })
    }

    fn send_mouse(
        &self,
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
        click: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(button) = click {
                return self.call_cmd(|reply| ActorCmd::MouseClick { button, reply }).await?;
            }
            // If currently connected, coalesce relative deltas without blocking;
            // if disconnected, route through cmd_tx so caller receives an accurate 503 if offline.
            if self.status_rx.borrow().connected {
                let mut pm = self.pending_mouse.lock().expect("pending mouse mutex poisoned");
                pm.dx = pm.dx.saturating_add(dx).clamp(-1270, 1270);
                pm.dy = pm.dy.saturating_add(dy).clamp(-1270, 1270);
                pm.vscroll = pm.vscroll.saturating_add(vscroll).clamp(-127, 127);
                pm.hscroll = pm.hscroll.saturating_add(hscroll).clamp(-127, 127);
                if buttons.is_some() {
                    pm.buttons = buttons;
                }
                pm.dirty = true;
                drop(pm);
                self.mouse_notify.notify_one();
                return Ok(());
            }
            self.call_cmd(|reply| ActorCmd::MouseMove { dx, dy, buttons, vscroll, hscroll, reply })
                .await?
        })
    }

    fn swipe_up(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.call_cmd(|reply| ActorCmd::SwipeUp { reply }).await? })
    }

    fn send_key(
        &self,
        mod_mask: u8,
        code: u8,
        shortcut: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.call_cmd(|reply| ActorCmd::Key { mod_mask, code, shortcut, reply }).await?
        })
    }

    fn send_text(&self, text: String, press_enter: bool) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move {
            self.call_cmd(|reply| ActorCmd::Text { text, press_enter, reply }).await?
        })
    }

    fn unlock_with_secret(
        &self,
        secret: Option<Zeroizing<String>>,
        wake_method: String,
        press_enter: bool,
        delay_ms: u64,
        use_preset: bool,
    ) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move {
            self.call_cmd(|reply| ActorCmd::Unlock {
                secret,
                wake_method,
                press_enter,
                delay_ms,
                use_preset,
                reply,
            })
            .await?
        })
    }

    fn unlock_preset_configured(&self) -> bool {
        self.has_preset
    }

    fn auto_unlock_enabled(&self) -> bool {
        self.auto_unlock
    }
}

struct AoaActor {
    telemetry: Arc<TelemetryStore>,
    filter: DeviceFilter,
    unlock_preset: Option<Zeroizing<String>>,
    auto_unlock: bool,
    auto_unlock_wake: String,
    cmd_rx: mpsc::Receiver<ActorCmd>,
    pending_mouse: Arc<Mutex<PendingMouse>>,
    mouse_notify: Arc<Notify>,
    status_tx: watch::Sender<BridgeStatus>,
    shutdown_rx: watch::Receiver<bool>,
    connected: Option<ConnectedDevice>,
    buttons: u8,
    last_error: Option<String>,
    last_probe_err: Option<String>,
    last_mouse_log: Instant,
    auto_unlocked_keys: HashSet<String>,
}

async fn next_hotplug(stream: &mut Option<nusb::hotplug::HotplugWatch>) -> Option<HotplugEvent> {
    match stream.as_mut() {
        Some(watch) => watch.next().await,
        None => std::future::pending().await,
    }
}

async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    loop {
        if rx.changed().await.is_err() || *rx.borrow() {
            return;
        }
    }
}

impl AoaActor {
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

    async fn fetch_str(dev: &Device, idx: Option<std::num::NonZeroU8>) -> String {
        let Some(index) = idx else {
            return String::new();
        };
        dev.get_string_descriptor(index, US_ENGLISH, Duration::from_millis(400))
            .await
            .unwrap_or_default()
    }

    async fn read_specs(info: &DeviceInfo, dev: &Device, aoa_version: u16) -> UsbSpecs {
        let desc = dev.device_descriptor();
        let usb_ver = desc.usb_version();
        let product = match info.product_string().map(str::to_string).filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => Self::fetch_str(dev, desc.product_string_index()).await,
        };
        let manufacturer =
            match info.manufacturer_string().map(str::to_string).filter(|s| !s.is_empty()) {
                Some(s) => s,
                None => Self::fetch_str(dev, desc.manufacturer_string_index()).await,
            };
        let serial = match info.serial_number().map(str::to_string).filter(|s| !s.is_empty()) {
            Some(s) => s,
            None => Self::fetch_str(dev, desc.serial_number_string_index()).await,
        };

        let mut usb_config = String::from("unknown");
        let mut max_power_ma = None;
        let mut interfaces = Vec::new();

        if let Ok(cfg) = dev.active_configuration() {
            let cfg_name = Self::fetch_str(dev, cfg.string_index()).await;
            if !cfg_name.is_empty() {
                usb_config = cfg_name;
            }
            max_power_ma = Some(usb_max_power_ma(usb_ver, cfg.max_power()));
            for grp in cfg.interfaces() {
                let alt = grp.first_alt_setting();
                let cls = alt.class();
                interfaces.push(UsbInterfaceSpec {
                    number: alt.interface_number(),
                    class_hex: format!("0x{cls:02x}"),
                    class_name: Self::usb_class_name(cls).to_string(),
                    subclass: alt.subclass(),
                    protocol: alt.protocol(),
                    name: Self::fetch_str(dev, alt.string_index()).await,
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

    async fn open_control(dev: &Device) -> std::result::Result<ControlHandle, String> {
        #[cfg(not(target_os = "windows"))]
        {
            Ok(ControlHandle::Direct(dev.clone()))
        }
        #[cfg(target_os = "windows")]
        {
            let mut candidates: Vec<(u8, u8)> = dev
                .active_configuration()
                .ok()
                .map(|cfg| {
                    cfg.interfaces()
                        .map(|g| {
                            let alt = g.first_alt_setting();
                            (alt.interface_number(), alt.class())
                        })
                        .collect()
                })
                .unwrap_or_default();
            if candidates.is_empty() {
                candidates.push((0, 0xFF));
            } else {
                // On Windows WinUSB is normally bound to the 0xFF vendor/ADB interface rather than MTP (0x06).
                candidates.sort_by_key(|&(_, cls)| u8::from(cls != 0xFF));
            }
            let mut last_err = String::new();
            for (iface_num, _) in candidates {
                match dev.claim_interface(iface_num).await {
                    Ok(intf) => return Ok(ControlHandle::Claimed(intf)),
                    Err(e) => last_err = format!("interface {iface_num}: {e}"),
                }
            }
            Err(format!("claim WinUSB interface ({last_err})"))
        }
    }

    async fn matches_filter(&self, dev: &DeviceInfo) -> bool {
        let vid = dev.vendor_id();
        let pid = dev.product_id();
        if vid == ROOT_HUB_VID || self.filter.ignore_vids.contains(&vid) {
            return false;
        }
        if self.filter.vid.is_none()
            && matches!(dev.class(), 0x01 | 0x03 | 0x07 | 0x09 | 0x0E | 0xE0)
        {
            return false;
        }
        if dev.class() == 0x09 {
            return false;
        }
        if self.filter.vid.is_some_and(|v| v != vid) || self.filter.pid.is_some_and(|p| p != pid) {
            return false;
        }
        if let Some(ref target_serial) = self.filter.serial {
            let actual = match dev.serial_number() {
                Some(s) => s.to_string(),
                None => match dev.get_serial_number_string().await {
                    Ok(Some(s)) => s,
                    _ => return false,
                },
            };
            if &actual != target_serial {
                return false;
            }
        }
        true
    }

    fn publish_status(&self) {
        let status = BridgeStatus {
            connected: self.connected.is_some(),
            device: self.connected.as_ref().map(|c| c.info.clone()),
            error: self.last_error.clone(),
        };
        self.status_tx.send_replace(status);
    }

    async fn disconnect(&mut self, unregister_usb: bool) {
        if let Some(conn) = self.connected.take() {
            if unregister_usb {
                release_and_unregister_hid(conn.transport.as_ref()).await;
            }
        }
        self.buttons = 0;
        self.pending_mouse.lock().expect("pending mouse mutex").dirty = false;
    }

    async fn ensure_connected(&mut self, force: bool) -> bool {
        if let Some(ref conn) = self.connected {
            if !force {
                let alive = conn
                    .transport
                    .control_in(ACCESSORY_GET_PROTOCOL, 0, 0, 2, Duration::from_millis(500))
                    .await
                    .is_ok();
                if alive {
                    return true;
                }
                self.telemetry.log("warn", "usb", "USB connection lost, re-probing", None);
            }
        }
        self.disconnect(force).await;

        let all_devs: Vec<DeviceInfo> = match nusb::list_devices().await {
            Ok(iter) => iter.collect(),
            Err(err) => {
                self.last_error = Some(format!("USB enumeration failed: {err}"));
                self.publish_status();
                return false;
            }
        };

        let mut listed = Vec::new();
        for d in all_devs {
            if self.matches_filter(&d).await {
                listed.push(d);
            }
        }

        if listed.is_empty() {
            self.auto_unlocked_keys.clear();
            let msg = "No Android USB device detected".to_string();
            if self.last_probe_err.as_deref() != Some(msg.as_str()) {
                self.telemetry.log("info", "usb", msg.clone(), None);
                self.last_probe_err = Some(msg.clone());
            }
            self.last_error = Some(msg);
            self.publish_status();
            return false;
        }

        let mut last_err = None;
        for dev_info in listed {
            let vid = dev_info.vendor_id();
            let pid = dev_info.product_id();
            let dev = match dev_info.open().await {
                Ok(d) => d,
                Err(e) => {
                    last_err =
                        Some(format_usb_open_hint(std::env::consts::OS, vid, pid, &e.to_string()));
                    continue;
                }
            };
            let ctrl: Arc<dyn UsbTransport> = match Self::open_control(&dev).await {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    last_err = Some(format_usb_open_hint(std::env::consts::OS, vid, pid, &e));
                    continue;
                }
            };
            let raw = match ctrl
                .control_in(ACCESSORY_GET_PROTOCOL, 0, 0, 2, Duration::from_millis(1000))
                .await
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

            let desc = dev.device_descriptor();
            if let Err(e) =
                register_hid_pair(ctrl.as_ref(), desc.usb_version(), desc.max_packet_size_0()).await
            {
                last_err = Some(format!("USB {vid:04x}:{pid:04x} HID register: {e}"));
                continue;
            }

            // Allow Android's f_accessory workqueue time to finish hid_add_device() before first event.
            if sleep_or_shutdown(&mut self.shutdown_rx, HID_SETTLE_DELAY).await {
                release_and_unregister_hid(ctrl.as_ref()).await;
                return false;
            }

            let specs = Self::read_specs(&dev_info, &dev, aoa_ver).await;
            let dev_key = format!(
                "{}:{}",
                specs.vid,
                if specs.serial.is_empty() { specs.bus.clone() } else { specs.serial.clone() }
            );
            self.telemetry.log(
                "ok",
                "aoa",
                format!(
                    "AOAv{} HID attached: {} [{}:{}]",
                    aoa_ver, specs.product, specs.vid, specs.pid
                ),
                Some(serde_json::json!({ "serial": specs.serial, "usb_config": specs.usb_config })),
            );

            self.connected = Some(ConnectedDevice {
                device_id: dev_info.id(),
                dev_key,
                transport: ctrl,
                info: specs,
            });
            self.buttons = 0;
            self.last_error = None;
            self.last_probe_err = None;
            self.publish_status();
            return true;
        }

        let err_msg = last_err.unwrap_or_else(|| "No AOAv2-capable USB device found".to_string());
        if self.last_probe_err.as_deref() != Some(err_msg.as_str()) {
            self.telemetry.log("warn", "usb", err_msg.clone(), None);
            self.last_probe_err = Some(err_msg.clone());
        }
        self.last_error = Some(err_msg);
        self.publish_status();
        false
    }

    async fn maybe_auto_unlock(&mut self) {
        if !self.auto_unlock {
            return;
        }
        let Some(dev_key) = self.connected.as_ref().map(|c| c.dev_key.clone()) else {
            return;
        };
        if self.auto_unlocked_keys.contains(&dev_key) {
            return;
        }
        if let Some(preset) = self.unlock_preset.clone() {
            let wake = self.auto_unlock_wake.clone();
            match self.unlock_secret(&preset, &wake, true, 600, true).await {
                Ok(_) => {
                    self.auto_unlocked_keys.insert(dev_key);
                }
                Err(err) => {
                    self.telemetry.log("error", "hid", format!("Auto-unlock failed: {err}"), None);
                }
            }
        }
    }

    async fn send_hid(&mut self, hid_id: u16, report: &[u8], kind: &str) -> Result<()> {
        if self.connected.is_none() && !self.ensure_connected(false).await {
            return Err(AppError::Usb(
                self.last_error.clone().unwrap_or_else(|| "No AOAv2 device connected".to_string()),
            ));
        }

        let transport = self.connected.as_ref().expect("connected ensured").transport.clone();
        let t0 = Instant::now();
        match send_hid_with_stall_retry(transport.as_ref(), hid_id, report, &mut self.shutdown_rx)
            .await
        {
            Ok(()) => {
                self.telemetry.record_hid(kind, t0.elapsed().as_secs_f64() * 1000.0);
                Ok(())
            }
            Err(UsbError::Disconnected | UsbError::Timeout) => {
                self.telemetry.log("warn", "usb", "USB handle reset, re-attaching", None);
                if !self.ensure_connected(true).await {
                    return Err(AppError::Usb(
                        self.last_error
                            .clone()
                            .unwrap_or_else(|| "USB re-enumeration failed".to_string()),
                    ));
                }
                let retry_transport =
                    self.connected.as_ref().expect("reconnected ensured").transport.clone();
                let t_retry = Instant::now();
                send_hid_with_stall_retry(
                    retry_transport.as_ref(),
                    hid_id,
                    report,
                    &mut self.shutdown_rx,
                )
                .await
                .map_err(|e| AppError::Usb(e.to_string()))?;
                self.telemetry.record_hid(kind, t_retry.elapsed().as_secs_f64() * 1000.0);
                Ok(())
            }
            Err(err) => Err(AppError::Usb(err.to_string())),
        }
    }

    async fn send_mouse_step(
        &mut self,
        dx: i32,
        dy: i32,
        buttons: Option<u8>,
        vscroll: i32,
        hscroll: i32,
    ) -> Result<()> {
        if let Some(b) = buttons {
            self.buttons = b & 0x1F;
        }
        let report = pack_mouse_report(self.buttons, dx, dy, vscroll, hscroll);
        self.send_hid(HID_MOUSE_ID, &report, "mouse").await?;
        if self.last_mouse_log.elapsed() >= Duration::from_secs(2) {
            self.last_mouse_log = Instant::now();
            self.telemetry.log(
                "info",
                "hid",
                "Mouse HID stream active",
                Some(serde_json::json!({ "buttons": self.buttons, "dx": dx, "dy": dy })),
            );
        }
        Ok(())
    }

    async fn flush_pending_mouse(&mut self) {
        let drained = {
            let mut pm = self.pending_mouse.lock().expect("pending mouse mutex");
            if !pm.dirty {
                return;
            }
            let d = (pm.dx, pm.dy, pm.buttons.take(), pm.vscroll, pm.hscroll);
            pm.dx = 0;
            pm.dy = 0;
            pm.vscroll = 0;
            pm.hscroll = 0;
            pm.dirty = false;
            d
        };
        let (mut rem_x, mut rem_y, mut btns, vs, hs) = drained;
        // Split coalesced deltas larger than 127 cleanly while streaming smoothly.
        loop {
            let sx = rem_x.clamp(-127, 127);
            let sy = rem_y.clamp(-127, 127);
            rem_x -= sx;
            rem_y -= sy;
            if let Err(err) = self.send_mouse_step(sx, sy, btns.take(), vs, hs).await {
                self.telemetry.log("error", "hid", format!("Mouse stream error: {err}"), None);
                break;
            }
            if rem_x == 0 && rem_y == 0 {
                break;
            }
        }
    }

    async fn send_mouse_click(&mut self, button: &str) -> Result<()> {
        let bit = match button {
            "left" => 1,
            "right" => 2,
            "middle" => 4,
            _ => return Err(AppError::BadRequest(format!("Unknown mouse click button: {button}"))),
        };
        self.send_hid(HID_MOUSE_ID, &pack_mouse_report(bit, 0, 0, 0, 0), "mouse").await?;
        let cancelled = sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(30)).await;
        let rel = self.send_hid(HID_MOUSE_ID, &pack_mouse_report(0, 0, 0, 0, 0), "mouse").await;
        self.buttons = 0;
        rel?;
        if cancelled {
            return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
        }
        self.telemetry.log("info", "hid", format!("Mouse {button} click"), None);
        Ok(())
    }

    async fn run_swipe_up(&mut self) -> Result<()> {
        let res: Result<()> = async {
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(MOD_SHIFT, 0), "key").await?;
            sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(30)).await;
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await?;
            sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(80)).await;
            for _ in 0..5 {
                self.send_hid(HID_MOUSE_ID, &pack_mouse_report(0, 0, 80, 0, 0), "mouse").await?;
                sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(10)).await;
            }
            self.send_hid(HID_MOUSE_ID, &pack_mouse_report(1, 0, 0, 0, 0), "mouse").await?;
            sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(30)).await;
            for _ in 0..12 {
                self.send_hid(HID_MOUSE_ID, &pack_mouse_report(1, 0, -40, 0, 0), "mouse").await?;
                sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(15)).await;
            }
            self.send_hid(HID_MOUSE_ID, &pack_mouse_report(0, 0, 0, 0, 0), "mouse").await?;
            Ok(())
        }
        .await;
        if res.is_err() {
            let _ = self.send_hid(HID_MOUSE_ID, &pack_mouse_report(0, 0, 0, 0, 0), "mouse").await;
            let _ = self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await;
        }
        self.buttons = 0;
        res
    }

    async fn send_key_event(
        &mut self,
        mut mod_mask: u8,
        mut code: u8,
        shortcut: Option<&str>,
    ) -> Result<()> {
        if let Some(name) = shortcut {
            let (m, c) = shortcut_to_hid(name)
                .ok_or_else(|| AppError::BadRequest(format!("Unknown shortcut: {name}")))?;
            mod_mask = m;
            code = c;
        }
        self.send_hid(HID_KEYB_ID, &pack_keyboard_report(mod_mask, code), "key").await?;
        let cancelled = sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(30)).await;
        let rel = self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await;
        rel?;
        if cancelled {
            return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
        }
        let label = shortcut
            .map(str::to_string)
            .unwrap_or_else(|| format!("mod=0x{mod_mask:02x} code=0x{code:02x}"));
        self.telemetry.log("info", "hid", format!("Key event: {label}"), None);
        Ok(())
    }

    async fn type_ascii(
        &mut self,
        text: &str,
        press_enter: bool,
        cadence_ms: u64,
    ) -> Result<usize> {
        let mut sent = 0usize;
        for ch in text.chars() {
            let Some((mod_mask, code)) = ascii_to_hid(ch) else {
                continue;
            };
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(mod_mask, code), "key").await?;
            sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(cadence_ms)).await;
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await?;
            if sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(cadence_ms)).await {
                return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
            }
            sent += 1;
        }
        if press_enter {
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0x28), "key").await?;
            sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(20)).await;
            self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await?;
        }
        Ok(sent)
    }

    async fn unlock_secret(
        &mut self,
        secret: &str,
        wake_method: &str,
        press_enter: bool,
        delay_ms: u64,
        preset: bool,
    ) -> Result<usize> {
        let normalized = Zeroizing::new(normalize_newlines(secret));
        let char_count = normalized.chars().count();
        if char_count == 0 || char_count > MAX_UNLOCK_CHARS {
            return Err(AppError::BadRequest(format!(
                "Unlock secret must be 1..{MAX_UNLOCK_CHARS} characters"
            )));
        }
        if let Err(bad) = validate_ascii_secret(&normalized) {
            return Err(AppError::BadRequest(format!(
                "Unlock secret contains unsupported non-ASCII character '{bad}'"
            )));
        }
        if !matches!(wake_method, "swipe" | "space" | "none") {
            return Err(AppError::BadRequest(
                "wake_method must be 'swipe', 'space', or 'none'".into(),
            ));
        }

        let clamped_delay = delay_ms.clamp(150, 3000);
        match wake_method {
            "swipe" => {
                self.run_swipe_up().await?;
                if sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(clamped_delay))
                    .await
                {
                    return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
                }
            }
            "space" => {
                for _ in 0..2 {
                    self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0x2C), "key").await?;
                    sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(30)).await;
                    self.send_hid(HID_KEYB_ID, &pack_keyboard_report(0, 0), "key").await?;
                    if sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(550)).await {
                        return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
                    }
                }
                if sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(clamped_delay))
                    .await
                {
                    return Err(AppError::ServiceUnavailable("Shutdown in progress".into()));
                }
            }
            _ => {}
        }

        let sent = self.type_ascii(&normalized, press_enter, 20).await?;
        self.telemetry.log(
            "info",
            "hid",
            format!(
                "Unlock sequence sent ({sent} chars, wake={wake_method}, enter={press_enter}, preset={preset})"
            ),
            None,
        );
        Ok(sent)
    }

    async fn handle_cmd(&mut self, cmd: ActorCmd) {
        match cmd {
            ActorCmd::Reconnect(reply) => {
                self.telemetry.record_reconnect();
                self.telemetry.log("info", "usb", "Manual USB OTG reconnect triggered", None);
                if self.ensure_connected(true).await {
                    self.maybe_auto_unlock().await;
                }
                let _ = reply.send(self.status_tx.borrow().clone());
            }
            ActorCmd::MouseMove { dx, dy, buttons, vscroll, hscroll, reply } => {
                let res = self.send_mouse_step(dx, dy, buttons, vscroll, hscroll).await;
                let _ = reply.send(res);
            }
            ActorCmd::MouseClick { button, reply } => {
                let res = self.send_mouse_click(&button).await;
                let _ = reply.send(res);
            }
            ActorCmd::SwipeUp { reply } => {
                let res = self.run_swipe_up().await;
                if res.is_ok() {
                    self.telemetry.log("info", "hid", "Macro executed: Swipe Up (unlock)", None);
                }
                let _ = reply.send(res);
            }
            ActorCmd::Key { mod_mask, code, shortcut, reply } => {
                let res = self.send_key_event(mod_mask, code, shortcut.as_deref()).await;
                let _ = reply.send(res);
            }
            ActorCmd::Text { text, press_enter, reply } => {
                let normalized = normalize_newlines(&text);
                let char_count = normalized.chars().count();
                let res = if char_count > MAX_TEXT_CHARS {
                    Err(AppError::BadRequest(format!(
                        "Text length exceeds {MAX_TEXT_CHARS} character limit"
                    )))
                } else {
                    match self.type_ascii(&normalized, press_enter, 15).await {
                        Ok(sent) => {
                            // Log character count only (never typed text/secrets)
                            self.telemetry.log(
                                "info",
                                "hid",
                                format!("Typed text ({sent} chars, enter={press_enter})"),
                                None,
                            );
                            Ok(sent)
                        }
                        Err(e) => Err(e),
                    }
                };
                let _ = reply.send(res);
            }
            ActorCmd::Unlock { secret, wake_method, press_enter, delay_ms, use_preset, reply } => {
                let res = if use_preset {
                    match self.unlock_preset.clone() {
                        Some(preset) => {
                            self.unlock_secret(&preset, &wake_method, press_enter, delay_ms, true)
                                .await
                        }
                        None => Err(AppError::BadRequest("No unlock preset configured".into())),
                    }
                } else {
                    match secret {
                        Some(sec) => {
                            self.unlock_secret(&sec, &wake_method, press_enter, delay_ms, false)
                                .await
                        }
                        None => Err(AppError::BadRequest(format!(
                            "Unlock secret must be 1..{MAX_UNLOCK_CHARS} characters"
                        ))),
                    }
                };
                let _ = reply.send(res);
            }
        }
    }

    async fn handle_hotplug(&mut self, ev: HotplugEvent) {
        match ev {
            HotplugEvent::Connected(dev) => {
                if dev.vendor_id() != ROOT_HUB_VID && self.matches_filter(&dev).await {
                    self.telemetry.log(
                        "info",
                        "usb",
                        format!(
                            "USB hotplug connected {:04x}:{:04x}",
                            dev.vendor_id(),
                            dev.product_id()
                        ),
                        None,
                    );
                    if !sleep_or_shutdown(&mut self.shutdown_rx, Duration::from_millis(250)).await
                        && self.connected.is_none()
                        && self.ensure_connected(false).await
                    {
                        self.maybe_auto_unlock().await;
                    }
                }
            }
            HotplugEvent::Disconnected(dev_id) => {
                if self.connected.as_ref().is_some_and(|c| c.device_id == dev_id) {
                    let old = self.connected.take().expect("checked");
                    self.auto_unlocked_keys.remove(&old.dev_key);
                    self.buttons = 0;
                    self.last_error = Some("USB device disconnected".into());
                    self.telemetry.log("warn", "usb", "Android USB device disconnected", None);
                    self.publish_status();
                }
            }
        }
    }

    async fn run(mut self) {
        if self.ensure_connected(false).await {
            self.maybe_auto_unlock().await;
        }
        let mut hotplug = match nusb::watch_devices() {
            Ok(w) => Some(w),
            Err(_) => {
                self.telemetry.log("info", "usb", "USB hotplug stream unavailable", None);
                None
            }
        };

        loop {
            tokio::select! {
                biased;
                _ = wait_shutdown(&mut self.shutdown_rx) => {
                    self.disconnect(true).await;
                    self.last_error = Some("Server shutting down".into());
                    self.publish_status();
                    break;
                }
                _ = self.mouse_notify.notified() => {
                    self.flush_pending_mouse().await;
                }
                maybe_cmd = self.cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(cmd) => self.handle_cmd(cmd).await,
                        None => {
                            self.disconnect(true).await;
                            break;
                        }
                    }
                }
                Some(ev) = next_hotplug(&mut hotplug) => {
                    self.handle_hotplug(ev).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedOut {
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    }

    struct MockUsbTransport {
        out_responses: Mutex<VecDeque<std::result::Result<(), UsbError>>>,
        recorded: Mutex<Vec<RecordedOut>>,
    }

    impl MockUsbTransport {
        fn new(responses: Vec<std::result::Result<(), UsbError>>) -> Self {
            Self {
                out_responses: Mutex::new(VecDeque::from(responses)),
                recorded: Mutex::new(Vec::new()),
            }
        }
    }

    impl UsbTransport for MockUsbTransport {
        fn control_in(
            &self,
            _request: u8,
            _value: u16,
            _index: u16,
            _length: u16,
            _timeout: Duration,
        ) -> BoxFuture<'_, std::result::Result<Vec<u8>, UsbError>> {
            Box::pin(async { Ok(vec![2, 0]) })
        }

        fn control_out<'a>(
            &'a self,
            request: u8,
            value: u16,
            index: u16,
            data: &'a [u8],
            _timeout: Duration,
        ) -> BoxFuture<'a, std::result::Result<(), UsbError>> {
            Box::pin(async move {
                self.recorded.lock().unwrap().push(RecordedOut {
                    request,
                    value,
                    index,
                    len: data.len(),
                });
                self.out_responses.lock().unwrap().pop_front().unwrap_or(Ok(()))
            })
        }
    }

    #[test]
    fn usb3_chunk_size_and_power_decode_accurately() {
        // USB 3.0 encodes 512 bytes as exponent 9; chunk size must clamp to 63, NOT 9.
        assert_eq!(aoa_hid_chunk_size(0x0310, 9), 63);
        assert_eq!(aoa_hid_chunk_size(0x0200, 64), 63);
        assert_eq!(aoa_hid_chunk_size(0x0200, 0), 8);
        assert_eq!(usb_max_power_ma(0x0300, 100), 800);
        assert_eq!(usb_max_power_ma(0x0200, 250), 500);
    }

    #[tokio::test]
    async fn stall_backoff_retries_without_re_registering_hid() {
        let mock = Arc::new(MockUsbTransport::new(vec![
            Err(UsbError::Stall),
            Err(UsbError::Stall),
            Ok(()),
        ]));
        let (_tx, mut shutdown_rx) = watch::channel(false);
        let res =
            send_hid_with_stall_retry(mock.as_ref(), 1, &[0, 5, 0, 0, 0], &mut shutdown_rx).await;
        assert!(res.is_ok());

        let rec = mock.recorded.lock().unwrap();
        assert_eq!(rec.len(), 3);
        assert!(rec.iter().all(|r| r.request == ACCESSORY_SEND_HID_EVENT));
        assert!(rec.iter().all(|r| r.request != ACCESSORY_UNREGISTER_HID));
    }

    #[tokio::test]
    async fn partial_hid_registration_cleans_up_mouse_on_keyboard_failure() {
        // Mouse sequence: unregister(ok), register(ok), set_desc chunk 1(ok), set_desc chunk 2(ok)
        // Keyboard sequence: unregister(ok), register(err)
        let mock = Arc::new(MockUsbTransport::new(vec![
            Ok(()),
            Ok(()),
            Ok(()),
            Ok(()),
            Ok(()),
            Err(UsbError::Other("fault".into())),
        ]));
        let res = register_hid_pair(mock.as_ref(), 0x0200, 64).await;
        assert!(res.is_err());

        let rec = mock.recorded.lock().unwrap();
        let last = rec.last().expect("cleanup recorded");
        assert_eq!(last.request, ACCESSORY_UNREGISTER_HID);
        assert_eq!(last.value, HID_MOUSE_ID);
    }

    #[tokio::test]
    async fn shutdown_cleanup_releases_and_unregisters_both_hid_ids() {
        let mock = Arc::new(MockUsbTransport::new(vec![]));
        release_and_unregister_hid(mock.as_ref()).await;
        let rec = mock.recorded.lock().unwrap();
        assert_eq!(rec.len(), 4);
        assert_eq!(
            rec[0],
            RecordedOut { request: ACCESSORY_SEND_HID_EVENT, value: 1, index: 0, len: 5 }
        );
        assert_eq!(
            rec[1],
            RecordedOut { request: ACCESSORY_SEND_HID_EVENT, value: 2, index: 0, len: 8 }
        );
        assert_eq!(
            rec[2],
            RecordedOut { request: ACCESSORY_UNREGISTER_HID, value: 1, index: 0, len: 0 }
        );
        assert_eq!(
            rec[3],
            RecordedOut { request: ACCESSORY_UNREGISTER_HID, value: 2, index: 0, len: 0 }
        );
    }

    #[test]
    fn usb_open_hints_are_actionable_per_os() {
        let linux = format_usb_open_hint("linux", 0x0e8d, 0x2008, "Permission denied (EACCES)");
        assert!(linux.contains("51-android.rules"));
        assert!(linux.contains("plugdev"));

        let win = format_usb_open_hint("windows", 0x0e8d, 0x2008, "claim WinUSB interface failed");
        assert!(win.contains("Zadig"));
        assert!(win.contains("usbipd-win"));

        let mac = format_usb_open_hint("macos", 0x18d1, 0x4ee7, "Exclusive access busy");
        assert!(mac.contains("OpenMTP"));

        let other = format_usb_open_hint("freebsd", 0x18d1, 0x4ee7, "timeout");
        assert_eq!(other, "USB 18d1:4ee7: timeout");
    }

    #[tokio::test]
    async fn actor_startup_and_shutdown_do_not_panic() {
        let tel = Arc::new(TelemetryStore::new(50, None).unwrap());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let filter = DeviceFilter { vid: Some(0xFFFF), pid: Some(0xFFFF), ..Default::default() };
        let (bridge, handle) =
            NusbAoaBridge::spawn(tel, filter, None, false, "swipe".into(), shutdown_rx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = bridge.status_snapshot();
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("actor exited cleanly")
            .expect("actor task did not panic");
    }
}
