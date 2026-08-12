# Framework Crate (Windows)

A native desktop GUI for Framework laptop fan control, battery charge limits, and live hardware telemetry. Built with Rust and [Iced](https://iced.rs/) 0.14, using [`framework_lib`](https://github.com/FrameworkComputer/framework-system) for direct EC hardware access.

Inspired by [framework-control](https://github.com/ozturkkl/framework-control) by [ozturkkl](https://github.com/ozturkkl).

## Features

- **Fan Control** — Auto (firmware), Manual (duty 0–100%), and Curve mode with interactive editor (4 draggable points, hysteresis, rate limiting)
- **Battery Management** — Max charge limit (30–100%) with toggle enable/disable, health, voltage, current display
- **Live Telemetry** — Real-time temperature chart (30s sliding window), per-sensor display with colored indicators, fan RPM in header
- **Misc Panel** — Keyboard backlight slider, fingerprint LED level, expansion card and USB-C port detection
- **About Page** — Hardware info (CPU, RAM, Display, BIOS) and software settings (poll rate, refresh interval)
- **System Tray** — Minimize to tray, tray icon with context menu (Show / Quit)

## Requirements

- **Platform**: Intel Core Ultra Series 1 (Meteor Lake) — only tested and supported on this platform
- Windows 10/11
- Administrator privileges (required for EC access)

## Build

```bash
cargo build --release
```

## Binary size checks

```powershell
# Debug build (larger because it keeps unoptimized code + debuginfo)
cargo build
Get-Item .\target\debug\framework-crate.exe | Select-Object Name,Length

# Release build (smaller, suitable for distribution)
cargo build --release
Get-Item .\target\release\framework-crate.exe | Select-Object Name,Length

# Compare debug vs release in one command
cargo build; Get-Item .\target\debug\framework-crate.exe | Select-Object Name,Length; Get-Item .\target\release\framework-crate.exe | Select-Object Name,Length
```

Notes:

- `debug` builds are intentionally larger because they keep full debug symbols and unoptimized code.
- Use `release` artifacts for packaging and distribution.
- If shipping a bundle, do not include the entire `target/` folder or `.pdb` files unless you need them for debugging.

## Run

```bash
# Must run as administrator
cargo run --release
```

## Architecture

```
src/
  main.rs              — Iced 0.14 application entry point, boot function
  app.rs               — App struct, Message handlers, mutate_config helper
  sub_state.rs         — AppState split into 6 grouped sub-state structs
  views.rs             — UI layout (sensors, fan control, battery, misc, settings)
  types.rs             — Config structs, FanControlMode, CurveConfig, validation
  style.rs             — Colors, fonts, layout constants
  config.rs            — TOML config load/save (atomic write via tmp+rename)
  config_save_task.rs  — Debounced config save (100ms) with battery apply
  background_task.rs   — EC polling loop, fan control, expansion/PD scans
  temp_chart.rs        — Canvas-based temperature line chart (30s history)
  curve_canvas.rs      — Canvas-based fan curve visualization
  fan_control.rs       — CurveStepper, rate limiting, duty calculation
  system_info.rs       — Windows API FFI (CPU, RAM, OS, display, tray)
  probe.rs             — HeightProbe widget for dynamic window sizing
  util.rs              — Time utilities, lock helpers (read_lock, with_write_lock)
  cli/
    ec_wrapper.rs      — EcClient wrapper around framework_lib's CrosEc
    mod.rs
  tray/
    mod.rs             — TrayManager (Windows tray icon)
    event.rs           — TrayIcon events
    message_pump.rs    — Dedicated message pump thread for tray icon
```

### Data Flow

```
framework_lib (CrosEc) → background_task → Arc<RwLock> → UI (view reads)
                                   ↕
                            config_save_task → config.toml
```

- **Hardware access**: `framework_lib` crate calls EC directly via kernel driver (no subprocess)
- **Config**: `dirs::config_dir() / framework-control/config.toml`
- **Background polling**: tokio task on LP-E core, 200ms–2s interval (idle slowdown)
- **UI refresh**: self-rescheduling tick (50–1000ms), idle/hidden slows to 1s/5s
- **Lock strategy**: `Arc<RwLock<Arc<T>>>` for shared state, narrow lock scope (<1µs)
- **State organization**: `AppState` split into `FanState`, `ThermalState`, `PeripheralState`, `BatteryState`, `SystemState`, `LifecycleState`

### Performance Optimizations

- `EcClient` shared via `Arc` — no hardware re-init on clone
- `ThermalData.temps` as `Arc<BTreeMap>` — history samples share data
- `SensorCache.sorted/colors` as `Arc<Vec>` — zero-copy per UI tick
- Config lock held <1µs (reads only needed fields, drops immediately)
- `curve_full_points` debounced (100ms after last slider edit)
- PD port history pushed only on data change
- Fan curve keeps running during idle (temperature response)
- `mutate_config` helper reduces boilerplate for config mutations

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

[telemetry]
poll_ms = 500
ui_refresh_ms = 100
selected_sensors = []
```

## Known Limitations

- **Platform-specific**: Currently only supports Intel Core Ultra Series 1 (Meteor Lake) Framework laptops. CPU identification uses x86_64 CPUID intrinsics.
- **EC driver**: Requires the Framework EC kernel driver to be installed for `framework_lib` to communicate with hardware.

## Third-Party Dependencies

- [`framework_lib`](https://github.com/FrameworkComputer/framework-system) — Framework EC hardware abstraction layer (from [framework-system](https://github.com/FrameworkComputer/framework-system))
- [`iced`](https://crates.io/crates/iced) — Cross-platform GUI framework for Rust

## Icon Attribution

The application icon is the "settings" icon (System category) from the [Iconoir](https://iconoir.com/) icon set, licensed under the [MIT License](https://github.com/iconoir-icons/iconoir/blob/master/LICENSE).

Rendering parameters: Optical Size 32, Stroke Weight 1.5, color `#7300ff` (R 115, G 0, B 255). The source SVG (`assets/settings.svg`) is reproduced with attribution to Iconoir.

## License

MIT
