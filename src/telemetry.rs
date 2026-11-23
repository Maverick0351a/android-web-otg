//! 100% local in-memory, SSE broadcast, and non-blocking optional JSONL telemetry store.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TryRecvError, TrySendError};
use std::sync::Mutex;
use std::thread::JoinHandle;
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
    last_hid_ms: f64,
    last_error: Option<String>,
}

struct FileSinkHandle {
    tx: SyncSender<String>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for FileSinkHandle {
    fn drop(&mut self) {
        // Worker drains once all SyncSender clones drop; join best-effort on shutdown.
        let (dummy_tx, _) = sync_channel(1);
        let dropped_tx = std::mem::replace(&mut self.tx, dummy_tx);
        drop(dropped_tx);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// Thread-safe local telemetry store with atomic counters, ring buffer, SSE broadcast, and JSONL sink.
pub struct TelemetryStore {
    inner: Mutex<InnerState>,
    started: Instant,
    mouse_packets: AtomicU64,
    key_packets: AtomicU64,
    reconnects: AtomicU64,
    errors: AtomicU64,
    ws_clients: AtomicUsize,
    tx: broadcast::Sender<TelemetryEvent>,
    log_tx: Option<SyncSender<String>>,
    _file_sink: Option<FileSinkHandle>,
}

impl TelemetryStore {
    /// Create a local telemetry store bounded to `max_events` (minimum 10).
    /// Fails eagerly at startup if `log_file` cannot be created or opened for appending.
    pub fn new(max_events: usize, log_file: Option<PathBuf>) -> std::io::Result<Self> {
        let mut log_tx = None;
        let mut file_sink = None;

        if let Some(path) = log_file {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let (sink_tx, sink_rx) = sync_channel::<String>(512);
            let mut writer = BufWriter::new(file);
            let worker =
                std::thread::Builder::new().name("otg-tel-log".into()).spawn(move || {
                    while let Ok(first) = sink_rx.recv() {
                        let _ = writeln!(writer, "{first}");
                        loop {
                            match sink_rx.try_recv() {
                                Ok(next) => {
                                    let _ = writeln!(writer, "{next}");
                                }
                                Err(TryRecvError::Empty) => {
                                    let _ = writer.flush();
                                    break;
                                }
                                Err(TryRecvError::Disconnected) => {
                                    let _ = writer.flush();
                                    return;
                                }
                            }
                        }
                    }
                    let _ = writer.flush();
                })?;
            log_tx = Some(sink_tx.clone());
            file_sink = Some(FileSinkHandle { tx: sink_tx, worker: Some(worker) });
        }

        let (tx, _) = broadcast::channel(256);
        Ok(Self {
            inner: Mutex::new(InnerState {
                events: VecDeque::with_capacity(max_events.max(10)),
                max_events: max_events.max(10),
                next_id: 1,
                hid_latencies: VecDeque::with_capacity(100),
                hid_timestamps: VecDeque::with_capacity(1024),
                last_hid_ms: 0.0,
                last_error: None,
            }),
            started: Instant::now(),
            mouse_packets: AtomicU64::new(0),
            key_packets: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            ws_clients: AtomicUsize::new(0),
            tx,
            log_tx,
            _file_sink: file_sink,
        })
    }

    /// Log a structured event into memory, broadcast listeners (SSE), and optional JSONL writer.
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
                self.errors.fetch_add(1, Ordering::Relaxed);
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

        if let Some(ref sink_tx) = self.log_tx {
            if let Ok(line) = serde_json::to_string(&event) {
                match sink_tx.try_send(line) {
                    Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
                }
            }
        }

        event
    }

    /// Record a transmitted HID packet (`mouse` or `key`) and its USB round-trip latency in ms.
    pub fn record_hid(&self, kind: &str, duration_ms: f64) {
        match kind {
            "mouse" => {
                self.mouse_packets.fetch_add(1, Ordering::Relaxed);
            }
            "key" => {
                self.key_packets.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let now = Instant::now();
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        let clamped_ms = duration_ms.max(0.0);
        st.last_hid_ms = clamped_ms;
        if st.hid_latencies.len() == 100 {
            st.hid_latencies.pop_front();
        }
        st.hid_latencies.push_back(clamped_ms);
        if st.hid_timestamps.len() == 1024 {
            st.hid_timestamps.pop_front();
        }
        st.hid_timestamps.push_back(now);
    }

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
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

    /// Return buffered events with `id > since_id` (used for SSE `Last-Event-ID` catch-up).
    #[must_use]
    pub fn events_since(&self, since_id: u64) -> Vec<TelemetryEvent> {
        let st = self.inner.lock().expect("telemetry mutex poisoned");
        st.events.iter().filter(|ev| ev.id > since_id).cloned().collect()
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
            mouse_packets: self.mouse_packets.load(Ordering::Relaxed),
            key_packets: self.key_packets.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
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

    /// Clear buffered events and return the next sequence ID so clients stay synced.
    pub fn clear(&self) -> u64 {
        let mut st = self.inner.lock().expect("telemetry mutex poisoned");
        st.events.clear();
        st.next_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn ring_buffer_and_filters_behave_cleanly() {
        let store = TelemetryStore::new(10, None).unwrap();
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
    fn hid_latency_metrics_and_background_jsonl_writer_work() {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let tmp = std::env::temp_dir().join(format!("android-web-otg-tel-{unique}.jsonl"));
        {
            let store = TelemetryStore::new(50, Some(tmp.clone())).unwrap();
            store.record_hid("mouse", 2.0);
            store.record_hid("key", 4.0);
            store.record_reconnect();
            store.log("ok", "aoa", "attached", None);

            let m = store.metrics();
            assert_eq!(m.mouse_packets, 1);
            assert_eq!(m.key_packets, 1);
            assert_eq!(m.reconnects, 1);
            assert!((m.avg_hid_ms - 3.0).abs() < 0.01);
            assert!((m.max_hid_ms - 4.0).abs() < 0.01);
        }
        // Dropping TelemetryStore flushes and joins background writer thread cleanly.
        let disk = fs::read_to_string(&tmp).expect("jsonl written");
        assert!(disk.contains("\"message\":\"attached\""));
        let _ = fs::remove_file(tmp);
    }

    #[test]
    fn unwritable_log_file_fails_fast() {
        let tmp_dir = std::env::temp_dir();
        let res = TelemetryStore::new(50, Some(tmp_dir));
        assert!(res.is_err());
    }
}
