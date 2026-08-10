# Framework Control (Windows)

A native desktop GUI for Framework laptop fan control, battery charge limits, and live hardware telemetry. Built with Rust and [Iced](https://iced.rs/) 0.13.

Inspired by [framework-control](https://github.com/ozturkkl/framework-control) by [ozturkkl](https://github.com/ozturkkl).

## Features

- **Fan Control** — Auto (firmware), Manual (duty %), and Curve mode with interactive editor (4 draggable points, hysteresis, rate limiting)
- **Battery Management** — Max charge limit (%) and charge rate limit (C) with toggle enable/disable, health, voltage, current display
- **Live Telemetry** — Real-time temperature chart (30s sliding window), per-sensor display with colored indicators, fan RPM in header
- **Misc Panel** — Keyboard backlight slider, fingerprint LED level, expansion card and USB-C port detection
- **About Page** — Hardware info (CPU, RAM, Display, BIOS) and software settings (poll rate, refresh interval)
- **Auto-download** — Downloads `framework_tool` from [FrameworkComputer/framework-system](https://github.com/FrameworkComputer/framework-system) when not found locally, with SHA256 verification

### Expansion Card Identification

Detects expansion card type via USB-C PD port status (`--pdports`) with state history tracking to distinguish USB-A cards from USB devices plugged into USB-C cards.

**Identification Logic:**

| Current Port State | History Check | Identification |
|-------------------|---------------|----------------|
| DP Alt Mode = Yes | — | DP/HDMI Expansion Card |
| Source + Dfp + no PD | Previously seen both Sink+Ufp and Source+Dfp for this port | USB-C Expansion Card (device plugged/unplugged) |
| Source + Dfp + no PD | Stable for ≥2 consecutive polls (only Source+Dfp seen) | USB-A Expansion Card |
| Source + Dfp + no PD | Unstable (<2 consecutive polls) | USB Device (newly connected) |
| Sink + Ufp + no PD | — | USB-C Expansion Card (idle) |
| Other states | — | USB-C Port |

**State History Tracking:**

The system maintains a history of the last 3 PD port states to improve detection accuracy:

- **USB-A vs USB Device**: A real USB-A Expansion Card consistently shows `Source + Dfp`. A USB device plugged into a USB-C card initially shows `Source + Dfp` but the history reveals the port was previously `Sink + Ufp` (idle), identifying it as a USB-C card with a connected device.

- **Stability threshold**: Requires 2+ consecutive identical states before classifying as USB-A Expansion Card, preventing false positives from transient connections.

DP/HDMI and Audio expansion card firmware versions are displayed via `--dp-hdmi-info` / `--audio-card-info`.

## Requirements

- **Platform**: Intel Core Ultra Series 1 (Meteor Lake) — only tested and supported on this platform
- Windows 10/11
- Administrator privileges (required by `framework_tool` for EC access)
- `framework_tool` — auto-downloaded on first run, or place `framework_tool.exe` next to the binary

## Build

```bash
cargo build --release
```

## Run

```bash
# Must run as administrator
cargo run --release
```

## Architecture

```
src/
  main.rs          — Iced application, UI layout, message handling
  types.rs         — Config structs, FanControlMode, CurveConfig, curve_full_points
  config.rs        — TOML config load/save (atomic write via tmp+rename)
  temp_chart.rs    — Canvas-based temperature line chart
  curve_canvas.rs  — Canvas-based fan curve visualization
  system_info.rs   — Windows API FFI (cpuid CPU, GlobalMemoryStatusEx RAM, RtlGetVersion OS, GetSystemMetrics display, Registry)
  cli/
    mod.rs
    framework_tool.rs        — CLI wrapper, download with SHA256 verification
    framework_tool_parser.rs — Parse thermal/power/versions/pdports/expansion card output
```

- **GUI ↔ Hardware**: Iced UI → `framework_tool` CLI subprocess → EC firmware
- **Config**: `dirs::config_dir() / framework-control/config.toml`
- **Data flow**: Background tokio task polls CLI, writes to `Arc<RwLock>`, UI reads synchronously
- **State tracking**: PD port history (`VecDeque<Vec<UsbCPort>>`) stores last 3 poll results for expansion card identification

## Configuration

Config file location: `%APPDATA%/framework-control/config.toml`

```toml
[fan]
mode = "curve"  # "disabled" | "manual" | "curve"

[fan.manual]
duty_pct = 50

[fan.curve]
poll_ms = 500
  [fan.curve.curve]
  points = [[40, 0], [60, 40], [75, 80], [85, 100]]
  hysteresis_c = 2
  rate_limit_pct_per_step = 10

[battery]
  [battery.charge_limit_max_pct]
  enabled = true
  value = 80

  [battery.charge_rate_c]
  enabled = false
  value = 0.5

[telemetry]
poll_ms = 500
ui_refresh_ms = 100
selected_sensors = []
```

## Known Limitations

- **Platform-specific**: Currently only supports Intel Core Ultra Series 1 (Meteor Lake) Framework laptops. CPU identification uses x86_64 CPUID intrinsics.
- **Download verification**: When `PINNED_SHA256` is not set (default), SHA256 verification relies solely on the GitHub API-provided digest. For maximum security, set `PINNED_SHA256` in `src/cli/framework_tool.rs` to a known-good hash.

## Third-Party Dependencies

This application uses [`framework_tool`](https://github.com/FrameworkComputer/framework-system) by Framework Computer Inc. for all hardware interactions (EC access, fan control, battery management, sensor readings). `framework_tool` is licensed under the [BSD 3-Clause License](https://github.com/FrameworkComputer/framework-system/blob/main/LICENSE.md).

## Icon Attribution

The application icon is the "settings" icon (System category) from the [Iconoir](https://iconoir.com/) icon set, licensed under the [MIT License](https://github.com/iconoir-icons/iconoir/blob/master/LICENSE).

Rendering parameters: Optical Size 32, Stroke Weight 1.5, color `#7300ff` (R 115, G 0, B 255). The source SVG (`assets/settings.svg`) is reproduced with attribution to Iconoir.

## License

MIT
