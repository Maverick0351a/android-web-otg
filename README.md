# android-web-otg

Browser-based **Android AOAv2 USB OTG Mouse, Keyboard, Hardware Specs, and Telemetry Bridge** written in pure Rust (`nusb` + `axum` + `tokio`).

Plug an Android phone with a broken touchscreen into any **headless Linux server, Raspberry Pi, or workstation**, open a browser tab over loopback or a trusted Tailscale/VPN tunnel, and steer the device immediately over **EP0 AOAv2 USB HID** — **with USB Debugging (`adb`) still OFF**, **zero `libusb`**, and **zero Python/Node runtime dependencies**.

```text
  Pre-ADB USB HID Control Plane (USB Debugging NOT required)
  [Browser UI] ──WS (/ws/hid) / HTTP──▶ [android-web-otg] ──EP0 AOAv2 HID──▶ [Android]
     • Free Trackpad + Optional Pointer Lock                              (MTP + existing
     • Masked PIN/Password Quick Unlock & Auto-Insert                      USB interfaces
     • Inline 3-Step Guide, Type & D-Pad, TalkBack (Broken LCD)            stay untouched)
     • USB & AOAv2 Hardware Specs Modal + Live SSE Telemetry
```

## Hardware Verification

Verified live against a physical broken-screen Android phone (**`PCD Bengal Max`**, USB `0e8d:2008`, MediaTek, High Speed 480 Mbps, 500 mA) running inside an unprivileged non-root Docker container (`--cap-drop=ALL`, `USER otg`):

- **AOAv2 HID attach:** Attached cleanly in `mtp` configuration (`AOA v2`) without claiming MTP or requiring `adb`.
- **Physical screen wakeup & HID response:** Observed directly on hardware that the broken phone's display lit up and responded live to WebSocket HID cursor movements and left clicks at **`0.42–0.60 ms` EP0 round-trip latency** with zero stalled transfers.
- **Clean shutdown:** `SIGINT` / `SIGTERM` unregisters both virtual HID devices (`hid=1` mouse, `hid=2` keyboard) cleanly (~`0.12s` container stop time).

## Demo & Screenshots

![Live Demo — Free Trackpad, Inline Guide, TalkBack & USB Specs Modal](docs/demo.gif)

| Cockpit Dashboard (Volcanic Obsidian UI) | USB & AOAv2 Hardware Specs Modal |
|---|---|
| ![android-web-otg Dashboard](docs/demo-dashboard.png) | ![USB & AOAv2 Hardware Specs Modal](docs/demo-specs-modal.png) |

- **Calm, popup-free controls:** **Free Trackpad** is active by default so dragging and tapping never trigger browser Pointer Lock banners unless you switch to **Pointer Lock** (`Esc` releases). **? Guide** expands inline below the header, and action confirmations appear quietly inline.
- **Pre-ADB inspection:** Click **Phone Specs** at any time to inspect USB `VID:PID`, AOAv2 protocol version, negotiated link speed, `bMaxPower`, bus/address, and active interface descriptors directly over USB.

## Why Pure-Rust `nusb` AOAv2 Over `scrcpy --otg`?

`scrcpy --otg` is ideal on a local desktop monitor, but it requires an SDL2/X11/Wayland GUI on the machine where the phone is plugged in and requires Zadig driver swaps on Windows. `android-web-otg` turns any **headless Linux/Pi USB bench** into a hardened browser control surface:

- **Zero external runtime deps:** Communicates directly with Linux `usbfs` through pure-Rust async `nusb` (`features = ["tokio"]`) managed by a single-owner `AoaActor` state machine — no `libusb`, no `adb`, no desktop environment.
- **EP0-only AOAv2 HID:** Uses USB Vendor Control Requests `51`, `54`, `55`, `56`, `57` without claiming bulk/interface endpoints or invoking `ACCESSORY_START`, leaving MTP and future `adb` interfaces free.
- **Resilient Android `f_accessory` timing:** Waits for HID report-descriptor registration settling (`200 ms`) and retries transient `Stall` errors with exponential backoff without unregistering HID IDs.
- **Low-latency WebSocket HID (`/ws/hid`) + REST fallback:** Coalesced mouse deltas (`dx`, `dy`, buttons, vertical/horizontal scroll), full HID keyboard forwarding, TalkBack navigation (`Alt+←`, `Alt+→`, `Alt+Enter`), and `Swipe Up` unlock gestures.
- **Masked Quick Unlock / Auto-Insert:** Optional sidebar unlock panel and single-shot USB-attach auto-insert (`ANDROID_WEB_OTG_UNLOCK_CODE` + `--auto-unlock`), logging character counts only (`zeroize` memory scrubbing; secrets never touch disk, URLs, or telemetry).
- **Non-blocking Telemetry & SSE Stream:** Lock-free atomic packet counters, HID round-trip latency (`avg` / `last` / `max`), bounded ring buffer (`/api/telemetry/stream` SSE with `Last-Event-ID`), optional background-thread JSONL logging (`--log-file`), and 1-click JSONL export.

## Cross-Platform Support, Permissions & Drivers

`android-web-otg` runs locally on the same computer where the phone is plugged in (`http://127.0.0.1:8095`) — **no separate Linux server is required**. Pure-Rust `nusb` provides the cross-OS USB transport adapter natively (`usbfs` on Linux, `IOKit` on macOS, `WinUSB` on Windows) with **zero `libusb` install**.

- **Linux (desktop, laptop, Raspberry Pi, headless server — plug & play):** Install [deploy/51-android.rules](deploy/51-android.rules) (`MODE="0660", GROUP="plugdev", TAG+="uaccess"` — covers major Android OEM VIDs plus any `ID_MTP_DEVICE`), reload udev, and join `plugdev` (or run from a local console session / rootless Docker). Linux accesses EP0 directly without detaching MTP or ADB.
- **Windows:**
  - **Option A — WSL2 + `usbipd-win` (recommended, zero Zadig):** Run `usbipd bind` and `usbipd attach --wsl` once, then run the Linux binary inside WSL2 with full native `usbfs` EP0 support and intact Windows MTP drivers.
  - **Option B — Native `android-web-otg.exe`:** Automatically probes all composite interfaces (preferring `0xFF` WinUSB). If Windows's built-in `WUDFWpdMtp` driver blocks raw EP0 transfers, bind `WinUSB` with Zadig (the same Windows kernel restriction as `scrcpy --otg`).
- **macOS (`aarch64-apple-darwin`):** Connects via `nusb` `IOKit` natively — ensure exclusive MTP utilities (Android File Transfer, OpenMTP) or `adb` aren't locking the interface if connection fails.

## Quickstart

### Build & Run from Source

```bash
cargo build --release
sudo cp deploy/51-android.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger

./target/release/android-web-otg --host 127.0.0.1 --port 8095
```

Open **`http://127.0.0.1:8095`** in your browser.

### Common CLI Flags

```bash
# Bind to a private VPN/Tailscale interface with mandatory API token authentication
export ANDROID_WEB_OTG_TOKEN="replace-with-strong-random-token"
android-web-otg --host <tailscale-or-lan-ip> --port 8095

# Pin to a specific device VID/PID or serial and persist local JSONL telemetry
android-web-otg --vid 0e8d --pid 2008 --log-file ./otg-telemetry.jsonl

# Single-shot auto-insert of unlock PIN/password once per newly attached phone
export ANDROID_WEB_OTG_UNLOCK_CODE="1234"
android-web-otg --auto-unlock --unlock-wake swipe
```

> **Tip for dead LCD screens:** Hold **Volume Up + Volume Down** on the phone for ~3 seconds to toggle Android **TalkBack**, then switch to the **TalkBack (Broken LCD)** tab in **Quick Controls** (`Alt+←`, `Alt+→`, `Alt+Enter`) to navigate by audio into Developer Options. *(AOAv2 HID sends physical mouse and keyboard events; video mirroring requires USB Debugging and lives in the companion multi-device Workbench roadmap.)*

## Deployment Templates

- **`udev` rules (`0660`, `plugdev`, `uaccess`):** [deploy/51-android.rules](deploy/51-android.rules)
- **Hardened `systemd` service:** [deploy/android-web-otg.service](deploy/android-web-otg.service) (reads `/etc/android-web-otg.env` for `ANDROID_WEB_OTG_TOKEN` / `ANDROID_WEB_OTG_UNLOCK_CODE`)
- **Least-privilege Docker container (`debian:bookworm-slim`, non-root `USER otg`, zero `--privileged`):**
  ```bash
  docker build -t android-web-otg -f deploy/Dockerfile .

  # Default container entrypoint listens on 0.0.0.0:8095 and requires ANDROID_WEB_OTG_TOKEN:
  docker run --rm --network host \
    --cap-drop=ALL \
    --device-cgroup-rule='c 189:* rmw' \
    -v /dev/bus/usb:/dev/bus/usb \
    --group-add "$(getent group plugdev | cut -d: -f3)" \
    -e ANDROID_WEB_OTG_TOKEN="replace-with-strong-random-token" \
    android-web-otg

  # Or override entrypoint for loopback-only testing without --token:
  docker run --rm --network host \
    --cap-drop=ALL \
    --device-cgroup-rule='c 189:* rmw' \
    -v /dev/bus/usb:/dev/bus/usb \
    --group-add "$(getent group plugdev | cut -d: -f3)" \
    --entrypoint /usr/local/bin/android-web-otg \
    android-web-otg --host 127.0.0.1 --port 8095
  ```

## Security Architecture

This service injects physical keyboard and mouse HID events into any matched Android phone attached over USB:

- **Refuses unauthenticated non-loopback binds:** Starting with a non-loopback `--host` (such as `0.0.0.0` or a LAN/VPN IP) without a non-empty `--token` / `ANDROID_WEB_OTG_TOKEN` exits immediately with an error.
- **Session cookie & header auth:** `POST /api/auth` validates the token using constant-time comparison (`subtle`) and issues an `HttpOnly; SameSite=Strict` CSPRNG session cookie. Query-string `?token=` URL parameters are intentionally unsupported to keep credentials out of browser history and logs.
- **DNS rebinding & CSWSH defense:** Every `/api/*` and `/ws/*` request validates the `Host` header against configured loopback/bind targets; state-changing HTTP routes and `/ws/hid` WebSocket upgrades enforce same-origin `Origin` and `Sec-Fetch-Site` headers.
- **Secret isolation:** Manual unlock codes and `ANDROID_WEB_OTG_UNLOCK_CODE` are ASCII-validated, normalized, wrapped in `Zeroizing`, and never stored in browser `localStorage`, JSON responses, or telemetry logs.

## Roadmap

Advanced post-ADB orchestration — including multi-device switching, `scrcpy-server` H.264/WebRTC video mirroring, click-to-tap calibration, ADB logcat/shell tools, and timestamped macro recording/replay — is intentionally scoped out of `android-web-otg` into a separate upcoming **Android USB Workbench** project so this crate stays a lean, focused, zero-`adb` OTG rescue utility.

## License

MIT — see [LICENSE](LICENSE).
