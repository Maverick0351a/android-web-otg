//! 100% local in-memory, SSE broadcast, and optional JSONL telemetry store.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// Structured event entry emitted to the UI stream and optional JSONL log file.
#[derive(Clone, Debug, Serialize)]
pub struct TelemetryEvent {
    pub id: u64,
    pub ts: f64,
    pub iso: String,
    pub level: String,
    pub category: String,
    pub message: String,
    pub detail: Option<Value>,
}

/// Rolling metrics summary displayed in the telemetry bar.
#[derive(Clone, Debug, Serialize)]
pub struct TelemetryMetrics {
    pub uptime_sec: u64,
    pub mouse_packets: u64,
    pub key_packets: u64,
    pub adb_actions: u64,
    pub screen_frames: u64,
    pub reconnects: u64,
    pub errors: u64,
    pub ws_clients: usize,
    pub last_hid_ms: f64,
    pub avg_hid_ms: f64,
    pub max_hid_ms: f64,
    pub events_per_sec: f64,
    pub last_error: Option<String>,
}

/// Combined snapshot payload returned by `GET /api/telemetry`.
#[derive(Clone, Debug, Serialize)]
pub struct TelemetrySnapshot {
    pub metrics: TelemetryMetrics,
    pub events: Vec<TelemetryEvent>,
    pub next_id: u64,
}

#[derive(Debug)]
struct InnerState {
    events: VecDeque<TelemetryEvent>,
    max_events: usize,
    next_id: u64,
    hid_latencies: VecDeque<f64>,
    hid_timestamps: VecDeque<Instant>,
    mouse_packets: u64,
    key_packets: u64,
    adb_actions: u64,
    screen_frames: u64,
    reconnects: u64,
    errors: u64,
    last_hid_ms: f64,
    last_error: Option<String>,
}

/// Thread-safe local telemetry store with ring buffer, broadcast channel, and optional JSONL sink.
pub struct TelemetryStore {
    inner: Mutex<InnerState>,
    started: Instant,
    ws_clients: AtomicUsize,
    tx: broadcast::Sender<TelemetryEvent>,
    log_path: Option<PathBuf>,
}

impl TelemetryStore {
    /// Create a local telemetry store bounded to `max_events` (minimum 10).
    #[must_use]
    pub fn new(max_events: usize, log_file: Option<PathBuf>) -> Self {
        let resolved_log = log_file.and_then(|path| {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() && fs::create_dir_all(parent).is_err() {
                    return None;
                }
            }
            Some(path)
        });
        let (tx, _) = broadcast::channel(128);
        Self {
            inner: Mutex::new(InnerState {
                events: VecDeque::with_capacity(max_events.max(10)),
                max_events: max_events.max(10),
                next_id: 1,
                hid_latencies: VecDeque::with_capacity(100),
                hid_timestamps: VecDeque::with_capacity(256),
                mouse_packets: 0,
                key_packets: 0,
                adb_actions: 0,
                screen_frames: 0,
                reconnects: 0,
                errors: 0,
                last_hid_ms: 0.0,
                last_error: None,
            }),
            started: Instant::now(),
            ws_clients: AtomicUsize::new(0),
            tx,
            log_path: resolved_log,
        }
    }

    /// Log a structured event into memory, broadcast listeners (SSE), and optional JSONL file.
    pub fn log(
        &self,
        level: &str,
        category: &str,
        message: impl Into<String>,
        detail: Option<Value>,
    ) -> TelemetryEvent {
        let msg = message.into();
        let now_sys = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let total_secs = now_sys.as_secs();
        let millis = now_sys.subsec_millis();
        let secs_day = total_secs % 86_400;
        let iso = format!(
            "{:02}:{:02}:{:02}.{:03}",
            secs_day / 3_600,
            (secs_day % 3_600) / 60,
            secs_day % 60,
            millis
        );
        let ts = (now_sys.as_secs_f64() * 1000.0).round() / 1000.0;

        let event = {
            let mut st = self.inner.lock().expect("telemetry mutex poisoned");
            let id = st.next_id;
            st.next_id += 1;
            if level == "error" {
                st.errors += 1;
                st.last_error = Some(msg.clone());
            }
            let ev = TelemetryEvent {
                id,
                ts,
                iso,
                level: level.to_string(),
                category: category.to_string(),
                message: msg,
                detail,
            };
            if st.events.len() == st.max_events {
                st.events.pop_front();
            }
            st.events.push_back(ev.clone());
            ev
        };

        let _ = self.tx.send(event.clone());

        if let Some(ref path) = self.log_path {
            if let Ok(mut fh) = OpenOptions::new().create(true).append(true).open(path) {
                if let Ok(line) = serde_json::to_string(&event) {
                    let _ = writeln!(fh, "{line}");
                }
            }
        }

        event
    }

    /// Record a transmitted HID packet (`mouse` or `key`) and its USB round-trip latency in ms.
    pub fn record_hid(&self, kind: &str, duration_ms: f64) {
        let now = Instant::now();
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        match kind {
            "mouse" => st.mouse_packets += 1,
            "key" => st.key_packets += 1,
            _ => {}
        }
        let clamped_ms = duration_ms.max(0.0);
        st.last_hid_ms = clamped_ms;
        if st.hid_latencies.len() == 100 {
            st.hid_latencies.pop_front();
        }
        st.hid_latencies.push_back(clamped_ms);
        if st.hid_timestamps.len() == 256 {
            st.hid_timestamps.pop_front();
        }
        st.hid_timestamps.push_back(now);
    }

    pub fn record_adb_action(&self) {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        st.adb_actions += 1;
    }

    pub fn record_screen_frame(&self) {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        st.screen_frames += 1;
    }

    pub fn record_reconnect(&self) {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        st.reconnects += 1;
    }

    pub fn ws_connected(&self) -> usize {
        self.ws_clients.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn ws_disconnected(&self) -> usize {
        self.ws_clients.fetch_sub(1, Ordering::Relaxed).saturating_sub(1)
    }

    /// Subscribe to real-time pushed events for SSE streaming.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryEvent> {
        self.tx.subscribe()
    }

    fn compute_metrics(&self, st: &mut InnerState) -> TelemetryMetrics {
        let now = Instant::now();
        while let Some(&front) = st.hid_timestamps.front() {
            if now.duration_since(front).as_secs_f64() > 5.0 {
                st.hid_timestamps.pop_front();
            } else {
                break;
            }
        }
        let events_per_sec = ((st.hid_timestamps.len() as f64 / 5.0) * 10.0).round() / 10.0;
        let (avg_hid_ms, max_hid_ms) = if st.hid_latencies.is_empty() {
            (0.0, 0.0)
        } else {
            let sum: f64 = st.hid_latencies.iter().sum();
            let max = st.hid_latencies.iter().copied().fold(0.0_f64, f64::max);
            (
                ((sum / st.hid_latencies.len() as f64) * 100.0).round() / 100.0,
                (max * 100.0).round() / 100.0,
            )
        };
        TelemetryMetrics {
            uptime_sec: self.started.elapsed().as_secs(),
            mouse_packets: st.mouse_packets,
            key_packets: st.key_packets,
            adb_actions: st.adb_actions,
            screen_frames: st.screen_frames,
            reconnects: st.reconnects,
            errors: st.errors,
            ws_clients: self.ws_clients.load(Ordering::Relaxed),
            last_hid_ms: (st.last_hid_ms * 100.0).round() / 100.0,
            avg_hid_ms,
            max_hid_ms,
            events_per_sec,
            last_error: st.last_error.clone(),
        }
    }

    #[must_use]
    pub fn metrics(&self) -> TelemetryMetrics {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        self.compute_metrics(&mut st)
    }

    /// Return metrics and filtered events with `id > since_id`.
    #[must_use]
    pub fn snapshot(
        &self,
        since_id: u64,
        category: Option<&str>,
        level: Option<&str>,
        limit: usize,
    ) -> TelemetrySnapshot {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        let metrics = self.compute_metrics(&mut st);
        let next_id = st.next_id;
        let mut events: Vec<TelemetryEvent> = st
            .events
            .iter()
            .filter(|ev| ev.id > since_id)
            .filter(|ev| match category {
                Some(cat) if !cat.is_empty() && cat != "all" => ev.category == cat,
                _ => true,
            })
            .filter(|ev| match level {
                Some(lvl) if !lvl.is_empty() && lvl != "all" => ev.level == lvl,
                _ => true,
            })
            .cloned()
            .collect();
        if limit > 0 && events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        TelemetrySnapshot { metrics, events, next_id }
    }

    #[must_use]
    pub fn export_jsonl(&self) -> String {
        let st = self.inner.lock().expect("telemetry mutex poisoned");
        let mut out = String::new();
        for ev in &st.events {
            if let Ok(line) = serde_json::to_string(ev) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    pub fn clear(&self) {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        st.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ring_buffer_and_filters_behave_cleanly() {
        let store = TelemetryStore::new(10, None);
        for i in 0..15 {
            let cat = if i % 2 == 0 { "usb" } else { "hid" };
            let lvl = if i == 14 { "error" } else { "info" };
            store.log(lvl, cat, format!("event-{i}"), None);
        }
        let snap = store.snapshot(0, Some("usb"), None, 50);
        assert!(snap.events.iter().all(|e| e.category == "usb"));
        assert_eq!(snap.metrics.errors, 1);

        let all = store.snapshot(0, None, None, 50);
        assert_eq!(all.events.len(), 10);
        assert_eq!(all.events.first().map(|e| e.id), Some(6));
    }

    #[test]
    fn hid_latency_metrics_and_jsonl_file_work() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let tmp = std::env::temp_dir().join(format!("android-web-otg-tel-{unique}.jsonl"));
        let store = TelemetryStore::new(50, Some(tmp.clone()));
        store.record_hid("mouse", 2.0);
        store.record_hid("key", 4.0);
        store.log("ok", "aoa", "attached", None);

        let m = store.metrics();
        assert_eq!(m.mouse_packets, 1);
        assert_eq!(m.key_packets, 1);
        assert!((m.avg_hid_ms - 3.0).abs() < 0.01);
        assert!((m.max_hid_ms - 4.0).abs() < 0.01);

        let disk = fs::read_to_string(&tmp).expect("jsonl written");
        assert!(disk.contains("\"message\":\"attached\""));
        let _ = fs::remove_file(tmp);
    }
}
