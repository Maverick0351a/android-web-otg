//! Pure-Rust optional ADB screen mirroring (`screenrecord` H.264 + `screencap` PNG fallback)
//! and calibrated touch tap/swipe injection once USB Debugging (`adb`) is enabled.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::error::{AppError, Result};
use crate::telemetry::TelemetryStore;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const STATUS_CACHE_TTL: Duration = Duration::from_millis(1500);
const MAX_ANNEX_B_BUF: usize = 2 * 1024 * 1024;
const MAX_COORD: u32 = 16_384;

/// High-level availability state for optional ADB screen mirroring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorState {
    Ready,
    Unauthorized,
    Offline,
    NoAdb,
    NoDevice,
}

/// Snapshot returned by `GET /api/mirror/status` and embedded in `GET /api/status`.
#[derive(Clone, Debug, Serialize)]
pub struct MirrorStatus {
    pub adb_installed: bool,
    pub state: MirrorState,
    pub serial: Option<String>,
    pub screen_width: Option<u32>,
    pub screen_height: Option<u32>,
    pub hint: Option<String>,
}

/// Requested streaming transport mode on `/ws/screen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorStreamMode {
    Auto,
    H264,
    Png,
}

impl MirrorStreamMode {
    #[must_use]
    pub fn from_query(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or("auto").to_ascii_lowercase().as_str() {
            "h264" => Self::H264,
            "png" => Self::Png,
            _ => Self::Auto,
        }
    }
}

/// Frame or metadata event produced by a live mirror session.
#[derive(Clone, Debug)]
pub enum ScreenChunk {
    Meta { engine: String, width: Option<u32>, height: Option<u32>, codec: Option<String> },
    H264 { is_keyframe: bool, data: Vec<u8> },
    Png(Vec<u8>),
    Error(String),
}

/// Abstract screen-mirroring backend (allowing local `adb` or clean-room custom servers + test mocks).
pub trait ScreenMirrorBackend: Send + Sync {
    fn status(&self, preferred_serial: Option<String>) -> BoxFuture<'_, MirrorStatus>;
    fn capture_png(&self, preferred_serial: Option<String>) -> BoxFuture<'_, Result<Vec<u8>>>;
    fn tap(&self, preferred_serial: Option<String>, x: u32, y: u32) -> BoxFuture<'_, Result<()>>;
    fn swipe(
        &self,
        preferred_serial: Option<String>,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        duration_ms: u32,
    ) -> BoxFuture<'_, Result<()>>;
    fn spawn_stream(
        &self,
        preferred_serial: Option<String>,
        mode: MirrorStreamMode,
        shutdown_rx: watch::Receiver<bool>,
    ) -> BoxFuture<'_, Result<mpsc::Receiver<ScreenChunk>>>;
}

/// Validate an ADB device serial so only safe identifier characters reach argv.
#[must_use]
pub fn sanitize_serial(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Parse `adb devices` stdout into `(serial, state)` pairs.
#[must_use]
pub fn parse_adb_devices(output: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("List of devices") || trimmed.starts_with('*')
        {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(serial_raw) = parts.next() else { continue };
        let Some(state_raw) = parts.next() else { continue };
        let Some(serial) = sanitize_serial(serial_raw) else { continue };
        out.push((serial, state_raw.to_ascii_lowercase()));
    }
    out
}

/// Choose the best ADB target from parsed devices, honoring an explicit/USB serial first.
#[must_use]
pub fn select_adb_target(
    devices: &[(String, String)],
    preferred_serial: Option<&str>,
) -> (MirrorState, Option<String>, Option<String>) {
    if let Some(pref) = preferred_serial.and_then(sanitize_serial) {
        if let Some((serial, st)) = devices.iter().find(|(s, _)| s == &pref) {
            return map_device_state(serial, st);
        }
    }
    if let Some((serial, _)) = devices.iter().find(|(_, st)| st == "device") {
        return (MirrorState::Ready, Some(serial.clone()), None);
    }
    if let Some((serial, st)) = devices.iter().find(|(_, st)| st == "unauthorized") {
        return map_device_state(serial, st);
    }
    if let Some((serial, st)) = devices.iter().find(|(_, st)| st == "offline") {
        return map_device_state(serial, st);
    }
    (
        MirrorState::NoDevice,
        None,
        Some(
            "No ADB device online. Use Free Trackpad or TalkBack first to enable USB Debugging for live screen mirroring."
                .to_string(),
        ),
    )
}

fn map_device_state(serial: &str, state: &str) -> (MirrorState, Option<String>, Option<String>) {
    match state {
        "device" => (MirrorState::Ready, Some(serial.to_string()), None),
        "unauthorized" => (
            MirrorState::Unauthorized,
            Some(serial.to_string()),
            Some(
                "ADB device is unauthorized — use Free Trackpad or Tab+Enter to accept 'Allow USB debugging' on the phone."
                    .to_string(),
            ),
        ),
        _ => (
            MirrorState::Offline,
            Some(serial.to_string()),
            Some("ADB device is offline — replug USB or wait a moment for ADB handshake.".into()),
        ),
    }
}

/// Parse `adb shell wm size` output (`Override size` takes precedence over `Physical size`).
#[must_use]
pub fn parse_wm_size(output: &str) -> Option<(u32, u32)> {
    let mut physical = None;
    let mut override_sz = None;
    for line in output.lines() {
        let lower = line.trim();
        if let Some(rest) = lower.strip_prefix("Override size:") {
            override_sz = parse_wxh(rest);
        } else if let Some(rest) = lower.strip_prefix("Physical size:") {
            physical = parse_wxh(rest);
        }
    }
    override_sz.or(physical)
}

fn parse_wxh(raw: &str) -> Option<(u32, u32)> {
    let (w_str, h_str) = raw.trim().split_once('x')?;
    let w = w_str.trim().parse::<u32>().ok()?;
    let h = h_str.trim().parse::<u32>().ok()?;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

fn find_start_code(buf: &[u8]) -> Option<(usize, usize)> {
    let len = buf.len();
    let mut i = 0;
    while i + 2 < len {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if i + 3 < len && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, 4));
            }
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

/// Incremental Annex-B H.264 NAL assembler that attaches cached SPS+PPS to every IDR keyframe.
#[derive(Default)]
pub struct AnnexBAssembler {
    buf: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    codec: Option<String>,
}

impl AnnexBAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn codec_string(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    /// Feed raw Annex-B bytes and collect completed access units `(is_keyframe, avc1_codec, data)`.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<(bool, Option<String>, Vec<u8>)> {
        if self.buf.len().saturating_add(chunk.len()) > MAX_ANNEX_B_BUF {
            self.buf.clear();
        }
        self.buf.extend_from_slice(chunk);

        let mut out = Vec::new();
        loop {
            let Some((first_idx, sc_len)) = find_start_code(&self.buf) else {
                if self.buf.len() > 4 {
                    let keep_from = self.buf.len() - 4;
                    self.buf.drain(..keep_from);
                }
                break;
            };
            if first_idx > 0 {
                self.buf.drain(..first_idx);
            }
            let Some((rel_next, _)) = find_start_code(&self.buf[sc_len..]) else {
                break;
            };
            let next_idx = sc_len + rel_next;
            let nal_body = self.buf[sc_len..next_idx].to_vec();
            self.buf.drain(..next_idx);
            if let Some(frame) = self.handle_nal(&nal_body) {
                out.push(frame);
            }
        }
        out
    }

    /// Flush any remaining buffered NAL unit at stream EOF.
    pub fn flush(&mut self) -> Vec<(bool, Option<String>, Vec<u8>)> {
        let mut out = Vec::new();
        if let Some((first_idx, sc_len)) = find_start_code(&self.buf) {
            if first_idx > 0 {
                self.buf.drain(..first_idx);
            }
            if self.buf.len() > sc_len {
                let nal_body = self.buf[sc_len..].to_vec();
                if let Some(frame) = self.handle_nal(&nal_body) {
                    out.push(frame);
                }
            }
        }
        self.buf.clear();
        out
    }

    fn handle_nal(&mut self, body: &[u8]) -> Option<(bool, Option<String>, Vec<u8>)> {
        let first_byte = *body.first()?;
        let nal_type = first_byte & 0x1F;
        match nal_type {
            7 => {
                let mut framed = Vec::with_capacity(4 + body.len());
                framed.extend_from_slice(&[0, 0, 0, 1]);
                framed.extend_from_slice(body);
                if body.len() >= 4 {
                    self.codec =
                        Some(format!("avc1.{:02X}{:02X}{:02X}", body[1], body[2], body[3]));
                }
                self.sps = Some(framed);
                None
            }
            8 => {
                let mut framed = Vec::with_capacity(4 + body.len());
                framed.extend_from_slice(&[0, 0, 0, 1]);
                framed.extend_from_slice(body);
                self.pps = Some(framed);
                None
            }
            5 => {
                let sps = self.sps.as_ref()?;
                let pps = self.pps.as_ref()?;
                let mut packet = Vec::with_capacity(sps.len() + pps.len() + 4 + body.len());
                packet.extend_from_slice(sps);
                packet.extend_from_slice(pps);
                packet.extend_from_slice(&[0, 0, 0, 1]);
                packet.extend_from_slice(body);
                Some((true, self.codec.clone(), packet))
            }
            1 => {
                if self.sps.is_none() || self.pps.is_none() {
                    return None;
                }
                let mut packet = Vec::with_capacity(4 + body.len());
                packet.extend_from_slice(&[0, 0, 0, 1]);
                packet.extend_from_slice(body);
                Some((false, self.codec.clone(), packet))
            }
            _ => None,
        }
    }
}

struct CachedStatus {
    key: Option<String>,
    at: Instant,
    status: MirrorStatus,
}

/// Native `tokio::process::Command` backend driving host `adb` with zero shell interpolation.
pub struct AdbMirrorBackend {
    adb_path: String,
    telemetry: Arc<TelemetryStore>,
    cache: Mutex<Option<CachedStatus>>,
}

impl AdbMirrorBackend {
    #[must_use]
    pub fn new(adb_path: String, telemetry: Arc<TelemetryStore>) -> Self {
        Self { adb_path, telemetry, cache: Mutex::new(None) }
    }
}

fn no_adb_status(err: &std::io::Error) -> MirrorStatus {
    let hint = if err.kind() == std::io::ErrorKind::NotFound {
        "Host `adb` binary not found in PATH (set --adb-path or ANDROID_WEB_OTG_ADB to enable optional screen mirroring)."
            .to_string()
    } else {
        format!("Failed to execute adb: {err}")
    };
    MirrorStatus {
        adb_installed: false,
        state: MirrorState::NoAdb,
        serial: None,
        screen_width: None,
        screen_height: None,
        hint: Some(hint),
    }
}

async fn run_adb_cmd(
    adb_path: &str,
    serial: Option<&str>,
    args: &[&str],
    timeout_dur: Duration,
) -> std::result::Result<(bool, Vec<u8>, String), std::io::Error> {
    let mut cmd = Command::new(adb_path);
    if let Some(s) = serial.and_then(sanitize_serial) {
        cmd.arg("-s").arg(s);
    }
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let output = match tokio::time::timeout(timeout_dur, cmd.output()).await {
        Ok(res) => res?,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("adb {} timed out", args.join(" ")),
            ));
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok((output.status.success(), output.stdout, stderr))
}

async fn probe_adb_status(adb_path: &str, preferred_serial: Option<&str>) -> MirrorStatus {
    let dev_res = run_adb_cmd(adb_path, None, &["devices"], Duration::from_secs(3)).await;
    let (ok, stdout, stderr) = match dev_res {
        Ok(triple) => triple,
        Err(err) => return no_adb_status(&err),
    };
    if !ok && stdout.is_empty() {
        return MirrorStatus {
            adb_installed: true,
            state: MirrorState::Offline,
            serial: None,
            screen_width: None,
            screen_height: None,
            hint: Some(if stderr.is_empty() {
                "adb devices returned an error".to_string()
            } else {
                stderr
            }),
        };
    }

    let text = String::from_utf8_lossy(&stdout);
    let parsed = parse_adb_devices(&text);
    let (state, serial, hint) = select_adb_target(&parsed, preferred_serial);

    let (screen_width, screen_height) = if state == MirrorState::Ready {
        if let Ok((true, sz_out, _)) = run_adb_cmd(
            adb_path,
            serial.as_deref(),
            &["shell", "wm", "size"],
            Duration::from_secs(3),
        )
        .await
        {
            let sz_txt = String::from_utf8_lossy(&sz_out);
            match parse_wm_size(&sz_txt) {
                Some((w, h)) => (Some(w), Some(h)),
                None => (None, None),
            }
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    MirrorStatus { adb_installed: true, state, serial, screen_width, screen_height, hint }
}

async fn capture_png_once(adb_path: &str, serial: Option<&str>) -> Result<Vec<u8>> {
    let res =
        run_adb_cmd(adb_path, serial, &["exec-out", "screencap", "-p"], Duration::from_secs(5))
            .await
            .map_err(|e| AppError::ServiceUnavailable(format!("adb screencap failed: {e}")))?;
    let (ok, stdout, stderr) = res;
    if !ok {
        let msg = if stderr.is_empty() { "adb screencap exited non-zero" } else { &stderr };
        return Err(AppError::ServiceUnavailable(msg.to_string()));
    }
    if stdout.len() < PNG_MAGIC.len() || stdout[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(AppError::ServiceUnavailable(
            "adb screencap did not return a valid PNG image".into(),
        ));
    }
    Ok(stdout)
}

async fn run_png_loop(
    adb_path: &str,
    serial: Option<&str>,
    tx: &mpsc::Sender<ScreenChunk>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    loop {
        if *shutdown_rx.borrow() || tx.is_closed() {
            break;
        }
        let started = Instant::now();
        match capture_png_once(adb_path, serial).await {
            Ok(png) => {
                if tx.try_send(ScreenChunk::Png(png)).is_err() && tx.is_closed() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx.send(ScreenChunk::Error(err.to_string())).await;
                break;
            }
        }
        let elapsed = started.elapsed();
        let pause = Duration::from_millis(120).saturating_sub(elapsed);
        if !pause.is_zero() {
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => break,
                _ = tx.closed() => break,
                () = tokio::time::sleep(pause) => {}
            }
        }
    }
}

async fn run_h264_once(
    adb_path: &str,
    serial: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    tx: &mpsc::Sender<ScreenChunk>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> std::result::Result<bool, String> {
    let mut cmd = Command::new(adb_path);
    if let Some(s) = serial.and_then(sanitize_serial) {
        cmd.arg("-s").arg(s);
    }
    cmd.args(["exec-out", "screenrecord", "--output-format=h264", "--bit-rate", "2000000", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn adb screenrecord: {e}"))?;
    let mut stdout =
        child.stdout.take().ok_or_else(|| "adb screenrecord stdout unavailable".to_string())?;

    let mut assembler = AnnexBAssembler::new();
    let mut read_buf = [0u8; 32 * 1024];
    let mut emitted_frames = false;
    let mut sent_codec_meta = false;

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                let _ = child.kill().await;
                return Ok(emitted_frames);
            }
            () = tx.closed() => {
                let _ = child.kill().await;
                return Ok(emitted_frames);
            }
            read_res = stdout.read(&mut read_buf) => {
                match read_res {
                    Ok(0) => break,
                    Ok(n) => {
                        for (is_keyframe, codec, data) in assembler.push(&read_buf[..n]) {
                            if !sent_codec_meta && codec.is_some() {
                                sent_codec_meta = true;
                                let _ = tx.try_send(ScreenChunk::Meta {
                                    engine: "h264".into(),
                                    width,
                                    height,
                                    codec,
                                });
                            }
                            emitted_frames = true;
                            let _ = tx.try_send(ScreenChunk::H264 { is_keyframe, data });
                        }
                    }
                    Err(e) => {
                        let _ = child.kill().await;
                        return Err(format!("screenrecord stream read failed: {e}"));
                    }
                }
            }
        }
    }

    for (is_keyframe, codec, data) in assembler.flush() {
        if !sent_codec_meta && codec.is_some() {
            sent_codec_meta = true;
            let _ = tx.try_send(ScreenChunk::Meta { engine: "h264".into(), width, height, codec });
        }
        emitted_frames = true;
        let _ = tx.try_send(ScreenChunk::H264 { is_keyframe, data });
    }

    let status = child.wait().await.map_err(|e| format!("adb wait failed: {e}"))?;
    if !emitted_frames && !status.success() {
        return Err("screenrecord exited without producing H.264 frames".into());
    }
    Ok(emitted_frames)
}

impl ScreenMirrorBackend for AdbMirrorBackend {
    fn status(&self, preferred_serial: Option<String>) -> BoxFuture<'_, MirrorStatus> {
        Box::pin(async move {
            let norm_key = preferred_serial.as_deref().and_then(sanitize_serial);
            {
                let guard = self.cache.lock().expect("mirror status mutex poisoned");
                if let Some(ref c) = *guard {
                    if c.key == norm_key && c.at.elapsed() < STATUS_CACHE_TTL {
                        return c.status.clone();
                    }
                }
            }
            let fresh = probe_adb_status(&self.adb_path, norm_key.as_deref()).await;
            {
                let mut guard = self.cache.lock().expect("mirror status mutex poisoned");
                *guard =
                    Some(CachedStatus { key: norm_key, at: Instant::now(), status: fresh.clone() });
            }
            fresh
        })
    }

    fn capture_png(&self, preferred_serial: Option<String>) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move {
            let st = self.status(preferred_serial).await;
            if st.state != MirrorState::Ready {
                return Err(AppError::ServiceUnavailable(
                    st.hint.unwrap_or_else(|| "ADB device not ready".into()),
                ));
            }
            capture_png_once(&self.adb_path, st.serial.as_deref()).await
        })
    }

    fn tap(&self, preferred_serial: Option<String>, x: u32, y: u32) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let st = self.status(preferred_serial).await;
            if st.state != MirrorState::Ready {
                return Err(AppError::ServiceUnavailable(
                    st.hint.unwrap_or_else(|| "ADB device not ready".into()),
                ));
            }
            let cx = x.min(MAX_COORD).to_string();
            let cy = y.min(MAX_COORD).to_string();
            let (ok, _, stderr) = run_adb_cmd(
                &self.adb_path,
                st.serial.as_deref(),
                &["shell", "input", "tap", &cx, &cy],
                Duration::from_secs(4),
            )
            .await
            .map_err(|e| AppError::ServiceUnavailable(format!("adb input tap failed: {e}")))?;
            if !ok {
                return Err(AppError::ServiceUnavailable(if stderr.is_empty() {
                    "adb input tap failed".into()
                } else {
                    stderr
                }));
            }
            self.telemetry.record_hid("mouse", 1.0);
            self.telemetry.log("info", "mirror", format!("ADB tap at ({cx}, {cy})"), None);
            Ok(())
        })
    }

    fn swipe(
        &self,
        preferred_serial: Option<String>,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        duration_ms: u32,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let st = self.status(preferred_serial).await;
            if st.state != MirrorState::Ready {
                return Err(AppError::ServiceUnavailable(
                    st.hint.unwrap_or_else(|| "ADB device not ready".into()),
                ));
            }
            let sx1 = x1.min(MAX_COORD).to_string();
            let sy1 = y1.min(MAX_COORD).to_string();
            let sx2 = x2.min(MAX_COORD).to_string();
            let sy2 = y2.min(MAX_COORD).to_string();
            let sdur = duration_ms.clamp(50, 3000).to_string();
            let (ok, _, stderr) = run_adb_cmd(
                &self.adb_path,
                st.serial.as_deref(),
                &["shell", "input", "swipe", &sx1, &sy1, &sx2, &sy2, &sdur],
                Duration::from_secs(5),
            )
            .await
            .map_err(|e| AppError::ServiceUnavailable(format!("adb input swipe failed: {e}")))?;
            if !ok {
                return Err(AppError::ServiceUnavailable(if stderr.is_empty() {
                    "adb input swipe failed".into()
                } else {
                    stderr
                }));
            }
            self.telemetry.record_hid("mouse", 1.0);
            self.telemetry.log(
                "info",
                "mirror",
                format!("ADB swipe ({sx1},{sy1}) -> ({sx2},{sy2}) in {sdur}ms"),
                None,
            );
            Ok(())
        })
    }

    fn spawn_stream(
        &self,
        preferred_serial: Option<String>,
        mode: MirrorStreamMode,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> BoxFuture<'_, Result<mpsc::Receiver<ScreenChunk>>> {
        Box::pin(async move {
            let st = self.status(preferred_serial).await;
            if st.state != MirrorState::Ready {
                return Err(AppError::ServiceUnavailable(
                    st.hint.unwrap_or_else(|| "ADB device not ready".into()),
                ));
            }

            let adb_path = self.adb_path.clone();
            let serial = st.serial.clone();
            let width = st.screen_width;
            let height = st.screen_height;
            let telemetry = self.telemetry.clone();
            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(async move {
                let engine_name = match mode {
                    MirrorStreamMode::Png => "png",
                    MirrorStreamMode::Auto | MirrorStreamMode::H264 => "h264",
                };
                let _ = tx
                    .send(ScreenChunk::Meta {
                        engine: engine_name.into(),
                        width,
                        height,
                        codec: None,
                    })
                    .await;

                if let Ok(first_png) = capture_png_once(&adb_path, serial.as_deref()).await {
                    let _ = tx.try_send(ScreenChunk::Png(first_png));
                }

                if mode == MirrorStreamMode::Png {
                    run_png_loop(&adb_path, serial.as_deref(), &tx, &mut shutdown_rx).await;
                    return;
                }

                let mut any_h264 = false;
                loop {
                    if *shutdown_rx.borrow() || tx.is_closed() {
                        break;
                    }
                    match run_h264_once(
                        &adb_path,
                        serial.as_deref(),
                        width,
                        height,
                        &tx,
                        &mut shutdown_rx,
                    )
                    .await
                    {
                        Ok(emitted) => {
                            if emitted {
                                any_h264 = true;
                            }
                            if !emitted && !any_h264 {
                                if mode == MirrorStreamMode::Auto {
                                    telemetry.log(
                                        "warn",
                                        "mirror",
                                        "screenrecord produced 0 frames — falling back to PNG mirror",
                                        None,
                                    );
                                    let _ = tx
                                        .send(ScreenChunk::Meta {
                                            engine: "png".into(),
                                            width,
                                            height,
                                            codec: None,
                                        })
                                        .await;
                                    run_png_loop(
                                        &adb_path,
                                        serial.as_deref(),
                                        &tx,
                                        &mut shutdown_rx,
                                    )
                                    .await;
                                } else {
                                    let _ = tx
                                        .send(ScreenChunk::Error(
                                            "screenrecord produced 0 H.264 frames; try Auto or PNG Safe mode"
                                                .into(),
                                        ))
                                        .await;
                                }
                                break;
                            }
                        }
                        Err(err) => {
                            if mode == MirrorStreamMode::Auto && !any_h264 {
                                telemetry.log(
                                    "warn",
                                    "mirror",
                                    format!(
                                        "H.264 stream unavailable ({err}) — falling back to PNG"
                                    ),
                                    None,
                                );
                                let _ = tx
                                    .send(ScreenChunk::Meta {
                                        engine: "png".into(),
                                        width,
                                        height,
                                        codec: None,
                                    })
                                    .await;
                                run_png_loop(&adb_path, serial.as_deref(), &tx, &mut shutdown_rx)
                                    .await;
                            } else {
                                let _ = tx.send(ScreenChunk::Error(err)).await;
                            }
                            break;
                        }
                    }

                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => break,
                        () = tx.closed() => break,
                        () = tokio::time::sleep(Duration::from_millis(150)) => {}
                    }
                }
            });

            Ok(rx)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_serials_safely() {
        assert_eq!(sanitize_serial(" ABC123._:-0 "), Some("ABC123._:-0".into()));
        assert_eq!(sanitize_serial(""), None);
        assert_eq!(sanitize_serial("bad serial"), None);
        assert_eq!(sanitize_serial("abc;rm -rf"), None);
    }

    #[test]
    fn parses_adb_devices_and_selects_targets() {
        let raw = "\
* daemon started successfully
List of devices attached
SER001\tunauthorized usb:1-2
SER002\tdevice usb:1-3 product:foo
SER003\toffline
";
        let devs = parse_adb_devices(raw);
        assert_eq!(devs.len(), 3);

        let (st, ser, _) = select_adb_target(&devs, Some("SER001"));
        assert_eq!(st, MirrorState::Unauthorized);
        assert_eq!(ser.as_deref(), Some("SER001"));

        let (st, ser, _) = select_adb_target(&devs, None);
        assert_eq!(st, MirrorState::Ready);
        assert_eq!(ser.as_deref(), Some("SER002"));

        let (st, ser, _) = select_adb_target(&[("SER003".into(), "offline".into())], None);
        assert_eq!(st, MirrorState::Offline);
        assert_eq!(ser.as_deref(), Some("SER003"));

        let (st, ser, _) = select_adb_target(&[], None);
        assert_eq!(st, MirrorState::NoDevice);
        assert_eq!(ser, None);
    }

    #[test]
    fn parses_wm_size_override_over_physical() {
        let phys_only = "Physical size: 1080x2400\n";
        assert_eq!(parse_wm_size(phys_only), Some((1080, 2400)));

        let with_override = "Physical size: 1080x2400\nOverride size: 720x1600\n";
        assert_eq!(parse_wm_size(with_override), Some((720, 1600)));
    }

    #[test]
    fn annex_b_assembler_prepends_sps_pps_to_idr_and_emits_deltas() {
        let mut asm = AnnexBAssembler::new();
        let mut stream = Vec::new();
        // SPS (type 7): profile 0x42, compat 0xE0, level 0x1E -> avc1.42E01E
        stream.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xE0, 0x1E, 0xAA]);
        // PPS (type 8)
        stream.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x3C]);
        // IDR (type 5)
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        // P-frame (type 1) split across pushes
        stream.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A]);

        let frames1 = asm.push(&stream);
        assert_eq!(frames1.len(), 1);
        assert!(frames1[0].0);
        assert_eq!(frames1[0].1.as_deref(), Some("avc1.42E01E"));
        assert!(frames1[0].2.starts_with(&[0, 0, 0, 1, 0x67]));

        let frames2 = asm.flush();
        assert_eq!(frames2.len(), 1);
        assert!(!frames2[0].0);
        assert_eq!(frames2[0].2, vec![0, 0, 0, 1, 0x41, 0x9A]);
    }
}
