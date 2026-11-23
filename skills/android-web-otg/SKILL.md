---
name: android-web-otg
description: >-
  Steer any broken-screen Android phone over USB using AOAv2 HID mouse/keyboard
  before USB Debugging is enabled, and capture/tap the live screen over ADB once
  debugging is authorized, via the built-in android-web-otg MCP server.
---

# `android-web-otg` Agent Skill

Use `android-web-otg --mcp` whenever the user needs help recovering, unlocking, inspecting, or automating a physical Android phone over USB — especially when the touchscreen is broken or **USB Debugging (`adb`) is turned off**.

---

## 1. Registering the Built-In MCP Server

Add `android-web-otg` (no external Python/Node runtime required) to your MCP client configuration (`mcp_config.json`, Claude Code `.mcp.json`, Cursor, Codex, or Gemini CLI):

```json
{
  "mcpServers": {
    "android-web-otg": {
      "command": "android-web-otg",
      "args": ["--mcp"],
      "env": {
        "ANDROID_WEB_OTG_ADB": "adb"
      }
    }
  }
}
```

- On Windows, use the full path to `android-web-otg.exe` and bind WinUSB with Zadig once if `WUDFWpdMtp` blocks raw AOAv2 EP0 transfers.
- On Linux, install `deploy/51-android.rules` and ensure your user is in `plugdev`.

---

## 2. Two-Phase Broken-Screen Workflow

### Phase 1 — Pre-ADB Rescue (AOAv2 USB HID Only)
When `otg_status` reports `usb.connected: true` and `mirror.state: "no_device"` or `"unauthorized"`:
1. Inspect hardware descriptors cleanly with **`otg_specs`** (`VID:PID`, AOAv2 version, speed, interfaces).
2. Wake the phone and bring up the lock/home prompt with **`otg_key`** (`{ "shortcut": "wake" }`) and **`otg_swipe_up`**.
3. Steer the on-device pointer with **`otg_mouse`** (`dx`, `dy`, `click: "left"|"right"|"middle"`, `vscroll`) and type non-secret navigation strings with **`otg_text`**.
4. **Dead or pitch-black LCD?** Ask the user to hold **Volume Up + Volume Down** for 3 seconds on the phone to toggle Android **TalkBack**, then step audibly through menus with `otg_key`:
   - `talkback_prev` (`Alt+Left`)
   - `talkback_next` (`Alt+Right`)
   - `talkback_activate` (`Alt+Enter`)
5. **Accepting ADB RSA Prompt:** If `mirror.state` is `"unauthorized"`, send `tab` -> `tab` -> `enter` (or steer the cursor / click `"Allow"`) with `otg_key` / `otg_mouse`.

### Phase 2 — Post-ADB Visual Automation
Once `otg_status` reports `mirror.state: "ready"`:
1. Call **`otg_screen_capture`** to inspect the live phone UI as an inline PNG image and note its pixel resolution.
2. Calculate exact coordinates from the captured image and execute **`otg_tap`** (`{ "x": ..., "y": ... }`) or **`otg_swipe`** (`{ "x1": ..., "y1": ..., "y2": ..., "duration_ms": 280 }`).
3. Combine **`otg_text`** and **`otg_key`** (`home`, `back`, `settings`, `notifications`) to finish backups, toggle MTP, or extract data.

---

## 3. Mandatory Secret Hygiene Rule

- **NEVER** ask the user to type their phone PIN/passcode into chat or pass a PIN through `otg_text` / MCP tool arguments (LLM prompts and MCP transcripts may be logged).
- Tell the user to open the local Web Cockpit (`http://127.0.0.1:8095`) or pass `ANDROID_WEB_OTG_UNLOCK_CODE` locally via the terminal for PIN/password unlock — those paths validate ASCII in Rust, wrap secrets in `Zeroizing`, and never log or persist passcodes.
