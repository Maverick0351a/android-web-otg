# android-web-otg — Architecture & Review Guide

Context rules for automated and human code reviews on `android-web-otg`.

## Scope (Strict v0.1 Boundary)
- **In scope:** Pure-Rust (`nusb` + `axum` + `tokio`) AOAv2 EP0-only HID mouse, keyboard, TalkBack navigation shortcuts, Phone Bezel Trackpad UI, Pre-ADB USB Hardware Specs modal, masked Quick Unlock / single-shot `--auto-unlock`, and 100% local telemetry (SSE + optional JSONL file sink).
- **Explicitly out of scope:** ADB screen mirroring, ADB system property queries, macro automation engines, and multi-device USB hub switching belong to the separate Android USB Workbench project. Do not reintroduce external `adb` binary or `libusb` runtime dependencies here.

## Architectural & Safety Invariants
1. **Single-owner async USB actor (`src/aoa.rs`):** Keep USB transport I/O and macro pacing strictly asynchronous inside `AoaActor` (`tokio::time::sleep` cancellable by `shutdown_rx`). Never hold a `std::sync::Mutex` across awaits/sleeps or call blocking `.wait()`.
2. **AOAv2 kernel settle & retry (`src/aoa.rs`):** Wait `HID_SETTLE_DELAY` after `ACCESSORY_SET_HID_REPORT_DESC`, and retry transient `UsbError::Stall` with bounded backoff **without** calling `ACCESSORY_UNREGISTER_HID` / re-registering HID. Preserve USB 3.0 `bMaxPacketSize0` exponent & `8 mA` power math.
3. **Clean HID shutdown:** `SIGINT` and Unix `SIGTERM` must signal `AoaActor` to release mouse/key state (`[0; 5]`, `[0; 8]`) and send `ACCESSORY_UNREGISTER_HID` before process exit.
4. **Anti-CSWSH / Anti-CSRF & Auth (`src/server.rs`):** Validate `Host`, `Sec-Fetch-Site`, and `Origin` (mandatory on `/ws/hid`) on every request, even when `--token` is unset. Never accept `?token=` in URL query strings; browser auth uses `POST /api/auth` (`HttpOnly; SameSite=Strict` CSPRNG session cookie), API clients use `Authorization: Bearer` / `X-Auth-Token`.
5. **Zero secret leakage:** Unlock/auth secrets must be wrapped in `Zeroizing<String>` where held in memory, rejected cleanly (`400 Bad Request`) if non-ASCII, and never echoed, stored in `localStorage`, or written to telemetry logs.
