# android-web-otg

Browser-based **Android AOAv2 USB OTG Mouse, Keyboard, Phone Specs, and Telemetry Bridge** written in pure Rust (`nusb` + `axum` + `tokio`).

Plug an Android phone with a broken touchscreen into any **headless Linux server, Raspberry Pi, or workstation**, open a browser tab from anywhere on your trusted LAN/Tailscale network, and drive the phone's cursor & keyboard immediately — **before USB Debugging (`adb`) is even enabled**, and with **zero `libusb` or Python runtime dependencies**.

```text
  Stage 1 (Pre-ADB, dead touch, USB Debugging OFF)
  [Browser UI] ──WS (/ws/hid) / HTTP──▶ [android-web-otg] ──EP0 AOAv2 HID──▶ [Android]
     • Realistic Phone Bezel Trackpad                                     (MTP + ADB
     • Masked PIN/Password Quick Unlock & Auto-Insert                      stay active)
     • USB & AOA Hardware Specs Modal + Live SSE Telemetry

  Stage 2 (Once you turn on USB Debugging + click "Always allow")
  [Browser UI] ◀──PNG Preview + Tap/Swipe──▶ [android-web-otg] ──USB/ADB──▶ [Android]
     • Full OS, SoC, RAM, storage, battery specs + live click-to-tap inside phone frame
```

## Why Not Just `scrcpy --otg`?

`scrcpy --otg` is fantastic on a local desktop, but it requires an SDL2/X11/Wayland GUI on the machine where the phone is plugged in and requires Zadig driver swaps on Windows. `android-web-otg` turns any **headless Linux/Pi USB bench** into a zero-install WebSocket + browser control surface:

- **Single static-friendly Rust binary:** Uses `nusb` directly against Linux `usbfs` — no `libusb`, no Python, no desktop environment required.
- **EP0-only AOAv2 HID:** Uses USB control requests `51`, `54`, `55`, `56`, `57` without claiming ADB interfaces or calling `ACCESSORY_START`, so MTP and `adb` stay active simultaneously on the same cable.
- **Low-latency WebSocket HID (`/ws/hid`) + REST fallback:** Smooth Pointer Lock mouse capture, Fixed Free-Move Trackpad mode, scroll/pan wheel, live keyboard forwarding, and TalkBack shortcuts.
- **Masked Quick Unlock / Auto-Insert:** Optional sidebar PIN/password panel and single-shot USB-attach auto-insert (`ANDROID_WEB_OTG_UNLOCK_CODE` + `--auto-unlock`), logging character counts only and never writing or echoing secrets.
- **Phone Specs Popup Modal:** Reads USB/AOAv2 descriptors immediately pre-ADB and unlocks full Android OS, SoC/ABI, display resolution/DPI, RAM, `/data` storage, battery/temperature, and kernel details once ADB is authorized.
- **100% Local Telemetry & Event Log:** Real-time SSE log stream (`/api/telemetry/stream`), HID round-trip latency (`avg` / `last`), packet rates, optional `--log-file` JSONL persistence, and 1-click JSONL export.

## Prerequisites

- **Linux host (recommended):** Kernel USB (`usbfs`) permissions via `deploy/51-android.rules` (`plugdev` group).
- **Optional post-ADB features:** `adb` in `PATH` enables Android system specs, live PNG screen preview, and tap/swipe injection inside the phone frame.
- **Windows hosts:** `nusb` on Windows requires a WinUSB-bound interface if Windows `WUDFWpdMtp` holds the phone exclusively; Linux hosts detach/share EP0 seamlessly out of the box.

## Quickstart

### Build & Run from Source

```bash
cargo build --release
sudo cp deploy/51-android.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger

./target/release/android-web-otg --host 127.0.0.1 --port 8095
```

Open **`http://127.0.0.1:8095`** in your browser.

### Useful CLI Flags

```bash
# Bind to a private VPN/Tailscale IP with API token authentication
export ANDROID_WEB_OTG_TOKEN="replace-with-strong-random-token"
android-web-otg --host 100.x.y.z --port 8095

# Target a specific USB device and persist local JSONL telemetry
android-web-otg --vid 0e8d --pid 2008 --log-file ./otg-telemetry.jsonl

# Single-shot auto-insert of unlock PIN/password once per newly connected phone
export ANDROID_WEB_OTG_UNLOCK_CODE="1234"
android-web-otg --auto-unlock --unlock-wake swipe
```

> **Tip for completely broken LCD displays:** Press and hold **Volume Up + Volume Down** on the phone for ~3 seconds to turn on Android **TalkBack**, then use the dedicated **TalkBack Navigation** buttons (`Alt+←`, `Alt+→`, `Alt+Enter`) to navigate audibly into Developer Options. *(Note: AOAv2 HID sends physical mouse/keyboard events; pre-ADB video mirroring is not possible until USB Debugging is authorized.)*

## Deployment Templates

- **Linux `udev` rules:** [deploy/51-android.rules](deploy/51-android.rules)
- **`systemd` unit:** [deploy/android-web-otg.service](deploy/android-web-otg.service)
- **Container (`Debian` + `adb`, single binary):**
  ```bash
  docker build -t android-web-otg -f deploy/Dockerfile .
  docker run --rm --privileged -v /dev/bus/usb:/dev/bus/usb -p 127.0.0.1:8095:8095 android-web-otg
  ```

## Security Notice

This tool injects full physical keyboard/mouse input into attached Android devices and can query ADB once authorized.

- Always bind to **`127.0.0.1`** or a private VPN/Tailscale interface protected by firewall rules.
- Set **`--token`** (`ANDROID_WEB_OTG_TOKEN`) whenever binding beyond loopback (`--host 0.0.0.0` or LAN/VPN IP); authentication uses constant-time comparison (`subtle`) across all `/api/*` and `/ws/*` routes.
- Unlock secrets typed in the UI or passed via `ANDROID_WEB_OTG_UNLOCK_CODE` are **never** saved to browser `localStorage`, returned in API responses, or written to telemetry logs.

## Future Rust Roadmap

- Native H.264 + WebRTC 60fps desktop-grade video inside the phone bezel via embedded `scrcpy-server`
- Pure-Rust USB/ADB client (`adb_client`) eliminating the external `adb` binary dependency
- Multi-device USB hub switcher & AOAv2 HID gamepad injection (`scrcpy -G` parity)

## License

MIT — see [LICENSE](LICENSE).
