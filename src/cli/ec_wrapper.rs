use framework_lib::chromium_ec::CrosEc;
use framework_lib::chromium_ec::CrosEcDriver;
use framework_lib::power;
use framework_lib::smbios;
use framework_lib::smbios::Platform;
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct ThermalData {
    pub temps: Arc<BTreeMap<String, i32>>,
    pub fans: SmallVec<[FanReading; 2]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FanReading {
    pub name: String,
    pub rpm: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryData {
    pub ac_present: Option<bool>,
    pub last_full_charge_capacity_mah: Option<u32>,
    pub remaining_capacity_mah: Option<u32>,
    pub remaining_capacity_wh: Option<f32>,
    pub soc_pct: Option<u32>,
    pub present_voltage_mv: Option<u32>,
    pub present_rate_ma: Option<u32>,
    pub charger_voltage_mv: Option<u32>,
    pub charger_current_ma: Option<u32>,
    pub chg_input_current_ma: Option<u32>,
    pub charger_temp_c: Option<f32>,
    pub design_capacity_mah: Option<u32>,
    pub design_capacity_wh: Option<f32>,
    pub cycle_count: Option<u32>,
    pub discharging: Option<bool>,
    pub manufacturer: Option<String>,
    pub model_number: Option<String>,
    pub serial_number: Option<String>,
    pub battery_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VersionsData {
    pub mainboard_type: Option<String>,
    pub mainboard_revision: Option<String>,
    pub uefi_version: Option<String>,
    pub uefi_release_date: Option<String>,
    pub ec_build_version: Option<String>,
    pub ec_current_image: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformFamily {
    Laptop12,
    Laptop13,
    Laptop16,
    Desktop,
    Unknown,
}

impl PlatformFamily {
    pub fn has_battery(self) -> bool {
        matches!(
            self,
            PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16
        )
    }
    pub fn has_fingerprint_led(self) -> bool {
        matches!(
            self,
            PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16
        )
    }
    pub fn has_keyboard_backlight(self) -> bool {
        matches!(
            self,
            PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16
        )
    }
}

pub fn platform_family(platform: Option<Platform>) -> PlatformFamily {
    match platform {
        Some(Platform::Framework12IntelGen13) | Some(Platform::Framework12IntelCore3) => {
            PlatformFamily::Laptop12
        }
        Some(Platform::IntelCoreUltra1)
        | Some(Platform::IntelCoreUltra3)
        | Some(Platform::IntelGen11)
        | Some(Platform::IntelGen12)
        | Some(Platform::IntelGen13)
        | Some(Platform::Framework13Amd7080)
        | Some(Platform::Framework13AmdAi300) => PlatformFamily::Laptop13,
        Some(Platform::Framework16Amd7080) | Some(Platform::Framework16AmdAi300) => {
            PlatformFamily::Laptop16
        }
        Some(Platform::FrameworkDesktopAmdAiMax300) => PlatformFamily::Desktop,
        Some(Platform::GenericFramework(..)) | Some(Platform::UnknownSystem) | None => {
            PlatformFamily::Unknown
        }
    }
}

pub fn detect_platform() -> PlatformFamily {
    platform_family(smbios::get_platform())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpansionCard {
    pub name: String,
    pub active_firmware: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsbCPort {
    pub port: u32,
    pub pd_contract: bool,
    pub power_role: Option<&'static str>,
    pub negotiated_text: Option<String>,
    pub negotiated_watts: Option<f32>,
    pub data_role: Option<&'static str>,
    pub dp_alt_mode: bool,
}

// PD port classification helpers.

const DISPLAY_CARD_WATTS_THRESHOLD: f32 = 3.0; // Above this is HDMI.
const HDMI_SOURCE_WATTS_MAX: f32 = 6.0; // Below 7.5W default charging.

fn is_low_power_display_source(watts: Option<f32>) -> bool {
    matches!(watts, Some(w) if w > 0.0 && w <= HDMI_SOURCE_WATTS_MAX)
}

fn classify_display_source(watts: Option<f32>, _prefer_named: bool) -> &'static str {
    match watts {
        Some(w) if w <= DISPLAY_CARD_WATTS_THRESHOLD => "DisplayPort Expansion Card",
        Some(_) => "HDMI Expansion Card",
        None => "DP/HDMI Expansion Card",
    }
}

fn role_is(port: &UsbCPort, role: &str) -> bool {
    port.power_role == Some(role)
}

fn is_pd_power_input(port: &UsbCPort) -> bool {
    port.pd_contract && role_is(port, "Sink")
}

fn is_source_no_pd(port: &UsbCPort) -> bool {
    !port.dp_alt_mode && role_is(port, "Source") && !port.pd_contract
}

fn is_sink_no_pd(port: &UsbCPort) -> bool {
    !port.dp_alt_mode && role_is(port, "Sink") && !port.pd_contract
}

fn history_has_role(
    history: &[&[UsbCPort]],
    port_id: u32,
    power_role: &str,
    pd_contract: bool,
) -> bool {
    history.iter().any(|h| {
        h.iter().any(|p| {
            p.port == port_id && p.power_role == Some(power_role) && p.pd_contract == pd_contract
        })
    })
}

fn same_port_identity(a: &UsbCPort, b: &UsbCPort) -> bool {
    a.port == b.port
        && a.pd_contract == b.pd_contract
        && a.power_role == b.power_role
        && a.dp_alt_mode == b.dp_alt_mode
}

pub fn classify_pd_port<'a>(
    port: &UsbCPort,
    history: impl IntoIterator<Item = &'a [UsbCPort]>,
    stable_threshold: usize,
    display_card_installed: bool,
    ever_seen_sink: bool,
) -> &'static str {
    const MAX_HIST: usize = 3;
    let empty: &[UsbCPort] = &[];
    let mut hist_buf: [&[UsbCPort]; MAX_HIST] = [empty; MAX_HIST];
    let mut hist_len: usize = 0;
    for h in history {
        if hist_len < MAX_HIST {
            hist_buf[hist_len] = h;
            hist_len += 1;
        }
    }
    let history = &hist_buf[..hist_len];

    if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!(
            "[classify] Port {}: role={:?}, pd_contract={}, dp_alt={}, watts={:?}, hist_len={}, display_card={}",
            port.port,
            port.power_role,
            port.pd_contract,
            port.dp_alt_mode,
            port.negotiated_watts,
            hist_len,
            display_card_installed
        );
    }

    if is_pd_power_input(port) {
        tracing::debug!(
            "[classify] Port {} → USB-C Expansion Card (Sink+PD)",
            port.port
        );
        return "USB-C Expansion Card";
    }
    if port.dp_alt_mode {
        let result = classify_display_source(port.negotiated_watts, true);
        tracing::debug!(
            "[classify] Port {} → {} (dp_alt, watts={:?})",
            port.port,
            result,
            port.negotiated_watts
        );
        return result;
    }
    if port.pd_contract && role_is(port, "Source") {
        // HDMI/DP can omit DP-alt but still draw ~3-5W Source; charging starts at 7.5W.
        if display_card_installed || is_low_power_display_source(port.negotiated_watts) {
            let result = classify_display_source(port.negotiated_watts, display_card_installed);
            tracing::debug!(
                "[classify] Port {} → {} (Source+PD, no dp_alt, watts={:?})",
                port.port,
                result,
                port.negotiated_watts
            );
            return result;
        }
        tracing::debug!(
            "[classify] Port {} → USB-C Expansion Card (Source+PD, no dp_alt, watts={:?})",
            port.port,
            port.negotiated_watts
        );
        return "USB-C Expansion Card";
    }
    if is_source_no_pd(port) {
        let has_seen_sink = history_has_role(history, port.port, "Sink", false)
            || history_has_role(history, port.port, "Sink", true);
        if has_seen_sink || ever_seen_sink {
            tracing::debug!(
                "[classify] Port {} → USB-C Expansion Card (Source+noPD, seen Sink)",
                port.port
            );
            return "USB-C Expansion Card";
        }
        let stable_count = history
            .iter()
            .filter(|h| h.iter().any(|p| same_port_identity(p, port)))
            .count();
        tracing::debug!(
            "[classify] Port {} Source+noPD: stable_count={}, threshold={}",
            port.port,
            stable_count,
            stable_threshold
        );
        if stable_count >= stable_threshold {
            tracing::debug!(
                "[classify] Port {} → USB-A Expansion Card (stable Source)",
                port.port
            );
            return "USB-A Expansion Card";
        }
        tracing::debug!(
            "[classify] Port {} → USB-C Expansion Card (Source+noPD, pending USB-A check)",
            port.port
        );
        return "USB-C Expansion Card";
    }
    if is_sink_no_pd(port) {
        tracing::debug!(
            "[classify] Port {} → USB-C Expansion Card (Sink+noPD)",
            port.port
        );
        return "USB-C Expansion Card";
    }
    tracing::debug!("[classify] Port {} → USB-C Port (fallback)", port.port);
    "USB-C Port"
}

pub struct EcClient {
    ec: CrosEc,
}

/// Shared handle slot for the EC client: `None` while uninitialized or
/// being re-created after a driver reset.
pub type SharedEc =
    std::sync::Arc<parking_lot::RwLock<std::sync::Arc<Option<std::sync::Arc<dyn EcHw>>>>>;

/// Hardware behind the EC wrapper. Object-safe (`&self` only) so callers
/// can hold `Arc<dyn EcHw>` and tests can substitute a scripted mock.
/// Production impl is `EcClient` below; every method body lives in the
/// `impl EcHw for EcClient` block so there is exactly one implementation.
pub trait EcHw: Send + Sync {
    fn thermal(&self) -> Result<ThermalData, String>;
    fn power(&self) -> Result<BatteryData, String>;
    fn versions(&self) -> Result<VersionsData, String>;
    fn set_fan_duty(&self, percent: u32, fan_index: Option<u32>) -> Result<(), String>;
    fn autofanctrl(&self) -> Result<(), String>;
    fn kblight_get(&self) -> Result<u32, String>;
    fn kblight_set(&self, percent: u32) -> Result<(), String>;
    fn fp_led_level_set(&self, level: &str) -> Result<(), String>;
    fn charge_limit_set(&self, min_pct: u8, max_pct: u8) -> Result<(), String>;
    fn pd_ports(&self) -> SmallVec<[UsbCPort; 4]>;
    fn expansion_cards(&self) -> SmallVec<[ExpansionCard; 4]>;
}

fn sensor_name_for_index(platform: Option<Platform>, index: usize) -> String {
    let name = match platform {
        Some(Platform::IntelGen11) | Some(Platform::IntelGen12) | Some(Platform::IntelGen13) => {
            match index {
                0 => "F75303_Local",
                1 => "F75303_CPU",
                2 => "F75303_DDR",
                3 => "Battery",
                4 => "PECI",
                5 if matches!(
                    platform,
                    Some(Platform::IntelGen12) | Some(Platform::IntelGen13)
                ) =>
                {
                    "F57397_VCCGT"
                }
                _ => return format!("Sensor {}", index),
            }
        }
        Some(Platform::IntelCoreUltra1) | Some(Platform::IntelCoreUltra3) => match index {
            0 => "F75303_Local",
            1 => "F75303_CPU",
            2 => "Battery",
            3 => "F75303_DDR",
            4 => "PECI",
            _ => return format!("Sensor {}", index),
        },
        Some(Platform::Framework12IntelGen13) => match index {
            0 => "F75303_CPU",
            1 => "F75303_Skin",
            2 => "F75303_Local",
            3 => "Battery",
            4 => "PECI",
            5 => "Charger IC",
            _ => return format!("Sensor {}", index),
        },
        Some(
            Platform::Framework13Amd7080
            | Platform::Framework13AmdAi300
            | Platform::Framework16Amd7080
            | Platform::Framework16AmdAi300,
        ) => {
            let is_16 = matches!(
                platform,
                Some(Platform::Framework16Amd7080) | Some(Platform::Framework16AmdAi300)
            );
            match index {
                0 => "F75303_Local",
                1 => "F75303_CPU",
                2 => "F75303_DDR",
                3 => "APU",
                4 if is_16 => "dGPU VR",
                5 if is_16 => "dGPU VRAM",
                6 if is_16 => "dGPU AMB",
                7 if is_16 => "dGPU temp",
                _ => return format!("Sensor {}", index),
            }
        }
        Some(Platform::FrameworkDesktopAmdAiMax300) => match index {
            0 => "F75303_APU",
            1 => "F75303_DDR",
            2 => "F75303_AMB",
            3 => "APU",
            4 => "Virtual",
            _ => return format!("Sensor {}", index),
        },
        _ => return format!("Sensor {}", index),
    };
    name.to_string()
}

/// Cached per-platform sensor names to avoid re-matching on every thermal poll.
type SensorNamesCache = parking_lot::Mutex<Option<(Option<Platform>, Vec<String>)>>;

static SENSOR_NAMES: std::sync::OnceLock<SensorNamesCache> = std::sync::OnceLock::new();

fn sensor_name(platform: Option<Platform>, index: usize) -> String {
    let cache = SENSOR_NAMES.get_or_init(|| parking_lot::Mutex::new(None));
    let mut guard = cache.lock();
    let (cached_platform, names) = guard.get_or_insert_with(|| (platform, Vec::new()));
    if *cached_platform != platform {
        *cached_platform = platform;
        names.clear();
    }
    if index >= names.len() {
        names.resize(index + 1, String::new());
    }
    if names[index].is_empty() {
        names[index] = sensor_name_for_index(platform, index);
    }
    names[index].clone()
}

impl EcClient {
    pub fn new() -> Result<Self, String> {
        let ec = CrosEc::new();
        ec.check_mem_magic()
            .map_err(|e| format!("EC initialization failed: {:?}", e))?;
        Ok(Self { ec })
    }
}

fn decode_ec_temperature(byte: u8) -> Option<i32> {
    match byte {
        0xFC..=0xFF => None,
        _ => {
            let temp = byte as i32 - 73;
            (-40..=125).contains(&temp).then_some(temp)
        }
    }
}

impl EcHw for EcClient {
    fn thermal(&self) -> Result<ThermalData, String> {
        let mut temps = BTreeMap::new();
        let mut fans = SmallVec::new();

        let platform = smbios::get_platform();

        if let Some(data) = self.ec.read_memory(0x00, 0x0F) {
            for (i, &byte) in data.iter().enumerate() {
                let Some(temp) = decode_ec_temperature(byte) else {
                    continue;
                };
                let name = sensor_name(platform, i);
                temps.insert(name, temp);
            }
        }

        if let Some(data) = self.ec.read_memory(0x10, 0x08) {
            for (i, chunk) in data.chunks(2).enumerate() {
                if chunk.len() == 2 {
                    let rpm = u16::from_le_bytes([chunk[0], chunk[1]]);
                    match rpm {
                        0xFFFF => continue,
                        0xFFFE => {
                            fans.push(FanReading {
                                name: format!("Fan {}", i + 1),
                                rpm: 0,
                            });
                        }
                        0 => {
                            fans.push(FanReading {
                                name: format!("Fan {}", i + 1),
                                rpm: 0,
                            });
                        }
                        r => {
                            fans.push(FanReading {
                                name: format!("Fan {}", i + 1),
                                rpm: r as u32,
                            });
                        }
                    }
                }
            }
        }

        Ok(ThermalData {
            temps: Arc::new(temps),
            fans,
        })
    }

    fn power(&self) -> Result<BatteryData, String> {
        let mut data = BatteryData::default();

        let info = power::power_info(&self.ec)
            .ok_or_else(|| "power_info returned None (no battery data available)".to_string())?;
        data.ac_present = Some(info.ac_present);
        if let Some(ref batt) = info.battery {
            data.present_voltage_mv = Some(batt.present_voltage);
            data.present_rate_ma = Some(batt.present_rate);
            data.remaining_capacity_mah = Some(batt.remaining_capacity);
            data.design_capacity_mah = Some(batt.design_capacity);
            data.design_capacity_wh =
                Some(batt.design_capacity as f32 * batt.design_voltage as f32 / 1_000_000.0);
            data.last_full_charge_capacity_mah = Some(batt.last_full_charge_capacity);
            data.cycle_count = Some(batt.cycle_count);
            data.soc_pct = Some(batt.charge_percentage);
            data.discharging = Some(batt.discharging);
            data.manufacturer = Some(batt.manufacturer.clone());
            data.model_number = Some(batt.model_number.clone());
            data.serial_number = Some(batt.serial_number.clone());
            data.battery_type = Some(batt.battery_type.clone());

            if batt.design_voltage > 0 {
                data.remaining_capacity_wh =
                    Some(batt.remaining_capacity as f32 * batt.design_voltage as f32 / 1_000_000.0);
            }
        }

        Ok(data)
    }

    fn versions(&self) -> Result<VersionsData, String> {
        let mut data = VersionsData {
            mainboard_type: smbios::get_product_name(),
            mainboard_revision: smbios::get_platform().map(|p| format!("{:?}", p)),
            ..Default::default()
        };

        if let Some((ro, rw, current_image)) = self.ec.flash_version() {
            let current = match current_image {
                framework_lib::chromium_ec::EcCurrentImage::RO => "RO",
                framework_lib::chromium_ec::EcCurrentImage::RW => "RW",
                _ => "Unknown",
            };
            data.ec_build_version = Some(if current == "RO" { ro } else { rw });
            data.ec_current_image = Some(current.to_string());
        }

        if let Some(esrt) = framework_lib::esrt::get_esrt() {
            for entry in esrt.entries.iter().take(esrt.resource_count as usize) {
                let version = format!(
                    "{:02X}.{:02X}",
                    (entry.fw_version >> 8) & 0xFF,
                    entry.fw_version & 0xFF
                );
                // Keep first fw_type == 1; later entries are not the system BIOS version.
                if entry.fw_type == 1 && data.uefi_version.is_none() {
                    data.uefi_version = Some(version);
                }
            }
        }

        Ok(data)
    }

    /// Sets fan duty, clamped to 100. Zero is allowed and means fan-off:
    /// callers pass it only from explicit user intent (Manual 0% target,
    /// quit duty) or user-configured curve points; stale-thermal paths
    /// hand back to firmware instead of writing a blind duty.
    fn set_fan_duty(&self, percent: u32, fan_index: Option<u32>) -> Result<(), String> {
        // Clamp at wrapper so no caller can send >100 to EC.
        let percent = percent.min(100);
        self.ec
            .fan_set_duty(fan_index, percent)
            .map_err(|e| format!("Failed to set fan duty: {:?}", e))
    }

    fn autofanctrl(&self) -> Result<(), String> {
        self.ec
            .autofanctrl(None)
            .map_err(|e| format!("Failed to set auto fan control: {:?}", e))
    }

    fn kblight_get(&self) -> Result<u32, String> {
        self.ec
            .get_keyboard_backlight()
            .map(|v| v as u32)
            .map_err(|e| format!("Failed to get keyboard backlight: {:?}", e))
    }

    fn kblight_set(&self, percent: u32) -> Result<(), String> {
        let percent = percent.min(100) as u8;
        // framework_lib ignores write errors; verify via readback with ±1 tolerance and brief retries for async EC update.
        self.ec.set_keyboard_backlight(percent);
        let mut last: Option<u8> = None;
        for _ in 0..5 {
            match self.ec.get_keyboard_backlight() {
                Ok(actual) if (actual as i32 - percent as i32).abs() <= 1 => return Ok(()),
                Ok(actual) => last = Some(actual),
                Err(e) => return Err(format!("Failed to set keyboard backlight: {:?}", e)),
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Err(format!(
            "Keyboard backlight write failed: wrote {}%, read back {:?}",
            percent, last
        ))
    }

    fn fp_led_level_set(&self, level: &str) -> Result<(), String> {
        use framework_lib::chromium_ec::commands::FpLedBrightnessLevel;
        let level_enum = match level.to_lowercase().as_str() {
            "high" => FpLedBrightnessLevel::High,
            "medium" => FpLedBrightnessLevel::Medium,
            "low" => FpLedBrightnessLevel::Low,
            "ultralow" => FpLedBrightnessLevel::UltraLow,
            "auto" => FpLedBrightnessLevel::Auto,
            _ => return Err(format!("Unknown FP LED level: {}", level)),
        };
        self.ec
            .set_fp_led_level(level_enum)
            .map_err(|e| format!("Failed to set FP LED level: {:?}", e))
    }

    fn charge_limit_set(&self, min_pct: u8, max_pct: u8) -> Result<(), String> {
        if min_pct > max_pct {
            return Err(format!(
                "Invalid charge limit range: min ({}) > max ({})",
                min_pct, max_pct
            ));
        }
        // Clamp at the API boundary so no caller (UI, quit-restore, init)
        // can push <25 into the EC even if validation is bypassed.
        // 100 means "no cap" per EC spec; 25 is the EC-enforced floor.
        // min==0 disables the hysteresis window and is passed through.
        let max_pct = max_pct.clamp(
            crate::style::CHARGE_LIMIT_MIN as u8,
            crate::style::CHARGE_LIMIT_MAX as u8,
        );
        let min_pct = if min_pct == 0 {
            0
        } else {
            min_pct
                .clamp(
                    crate::style::CHARGE_LIMIT_MIN as u8,
                    crate::style::CHARGE_LIMIT_MAX as u8,
                )
                .min(max_pct)
        };
        self.ec
            .set_charge_limit(min_pct, max_pct)
            .map_err(|e| format!("Failed to set charge limit: {:?}", e))?;
        // framework_lib reports success for the EC command, but the EC can
        // still ignore or clamp the value. Read it back like the keyboard
        // backlight path, allowing a short update window and ±1 rounding.
        // Keep the retry window small: this runs inside one EC dispatcher op,
        // and the startup caller bounds the whole set+readback to 1.5s.
        let mut last_readback = None;
        for _ in 0..3 {
            match self.ec.get_charge_limit() {
                Ok((read_min, read_max)) => {
                    if read_max.abs_diff(max_pct) <= 1 {
                        return Ok(());
                    }
                    last_readback = Some((read_min, read_max));
                }
                Err(e) => {
                    // Older firmware may not implement the read command.
                    // The write itself succeeded, so do not fail the caller.
                    tracing::debug!("Charge limit readback unavailable: {:?}", e);
                    return Ok(());
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(format!(
            "Charge limit write failed: wrote {}%, read back {:?}",
            max_pct, last_readback
        ))
    }

    fn pd_ports(&self) -> SmallVec<[UsbCPort; 4]> {
        use framework_lib::chromium_ec::EcRequestRaw;
        use framework_lib::chromium_ec::commands::EcRequestGetPdPortState;

        let mut ports = SmallVec::new();
        // Laptop 16 has six expansion bays; querying an absent port only
        // produces a debug log, so the wider count is safe and avoids
        // silently hiding the last two ports.
        let port_count = match detect_platform() {
            PlatformFamily::Laptop16 => 6,
            _ => 4,
        };
        for i in 0u8..port_count {
            let info = match (EcRequestGetPdPortState { port: i }).send_command(&self.ec) {
                Ok(info) => info,
                Err(e) => {
                    tracing::debug!("pd_ports: port {} query failed: {:?}", i, e);
                    continue;
                }
            };
            let c_state = info.c_state;
            let pd_state = info.pd_state;
            let raw_role = info.power_role;
            let raw_data = info.data_role;
            let voltage = info.voltage;
            let current = info.current;
            let dp_alt_raw = info.pd_alt_mode_status;

            let power_role = match raw_role {
                0 => "Sink",
                1 => "Source",
                _ => "Unknown",
            };
            let data_role = match raw_data {
                0 => "Ufp",
                1 => "Dfp",
                _ => "Disconnected",
            };
            let watts_mw = ((voltage as u64 * current as u64) / 1000) as u32;
            let negotiated_watts = if watts_mw > 0 {
                Some(watts_mw as f32 / 1000.0)
            } else {
                None
            };
            let negotiated_text = if voltage > 0 && current > 0 {
                Some(format!(
                    "PD {:.0}W, {:.0}V, {:.2}A",
                    watts_mw as f32 / 1000.0,
                    voltage as f32 / 1000.0,
                    current as f32 / 1000.0
                ))
            } else {
                None
            };

            tracing::debug!(
                "[pd_ports] Port {}: c_state={}, pd_state={}, role={}, data_role={}, v={}mV i={}mA dp_alt=0x{:02X} watts={:?}",
                i,
                c_state,
                pd_state,
                power_role,
                data_role,
                voltage,
                current,
                dp_alt_raw,
                negotiated_watts
            );

            ports.push(UsbCPort {
                port: i as u32,
                pd_contract: pd_state != 0,
                power_role: Some(power_role),
                negotiated_text,
                negotiated_watts,
                data_role: Some(data_role),
                dp_alt_mode: (dp_alt_raw & 0x03) != 0,
            });
        }
        ports
    }

    fn expansion_cards(&self) -> SmallVec<[ExpansionCard; 4]> {
        SmallVec::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn ec_temperature_decoder_keeps_cold_readings_and_drops_sentinels() {
        assert_eq!(decode_ec_temperature(0xFC), None);
        assert_eq!(decode_ec_temperature(0xFF), None);
        assert_eq!(decode_ec_temperature(0x00), None);
        assert_eq!(decode_ec_temperature(73 - 5), Some(-5));
        assert_eq!(decode_ec_temperature(73), Some(0));
        assert_eq!(decode_ec_temperature(73 + 125), Some(125));
        assert_eq!(decode_ec_temperature(73 + 126), None);
    }

    #[test]
    fn framework12_core3_maps_to_laptop12() {
        assert_eq!(
            platform_family(Some(Platform::Framework12IntelCore3)),
            PlatformFamily::Laptop12
        );
    }

    #[test]
    fn platform_family_covers_all_known_platforms() {
        let cases = [
            (Platform::Framework12IntelGen13, PlatformFamily::Laptop12),
            (Platform::Framework12IntelCore3, PlatformFamily::Laptop12),
            (Platform::IntelGen11, PlatformFamily::Laptop13),
            (Platform::IntelGen12, PlatformFamily::Laptop13),
            (Platform::IntelGen13, PlatformFamily::Laptop13),
            (Platform::IntelCoreUltra1, PlatformFamily::Laptop13),
            (Platform::IntelCoreUltra3, PlatformFamily::Laptop13),
            (Platform::Framework13Amd7080, PlatformFamily::Laptop13),
            (Platform::Framework13AmdAi300, PlatformFamily::Laptop13),
            (Platform::Framework16Amd7080, PlatformFamily::Laptop16),
            (Platform::Framework16AmdAi300, PlatformFamily::Laptop16),
            (
                Platform::FrameworkDesktopAmdAiMax300,
                PlatformFamily::Desktop,
            ),
            (Platform::UnknownSystem, PlatformFamily::Unknown),
        ];
        for (platform, expected) in cases {
            assert_eq!(platform_family(Some(platform)), expected, "{platform:?}");
        }
        assert_eq!(platform_family(None), PlatformFamily::Unknown);
    }

    fn port(role: &'static str, pd: bool, dp_alt: bool, watts: Option<f32>) -> UsbCPort {
        UsbCPort {
            port: 1,
            pd_contract: pd,
            power_role: Some(role),
            negotiated_text: None,
            negotiated_watts: watts,
            data_role: Some("Dfp"),
            dp_alt_mode: dp_alt,
        }
    }

    #[test]
    fn hdmi_source_without_dp_alt_is_hdmi() {
        let p = port("Source", true, false, Some(3.4));
        assert_eq!(
            classify_pd_port(&p, std::iter::empty(), 2, false, false),
            "HDMI Expansion Card"
        );
    }

    #[test]
    fn displayport_low_watt_source() {
        let p = port("Source", true, true, Some(2.5));
        assert_eq!(
            classify_pd_port(&p, std::iter::empty(), 2, false, false),
            "DisplayPort Expansion Card"
        );
    }

    #[test]
    fn usbc_default_source_stays_usbc() {
        let p = port("Source", true, false, Some(7.5));
        assert_eq!(
            classify_pd_port(&p, std::iter::empty(), 2, false, false),
            "USB-C Expansion Card"
        );
    }

    #[test]
    fn sink_pd_is_usbc() {
        let p = port("Sink", true, false, Some(65.0));
        assert_eq!(
            classify_pd_port(&p, std::iter::empty(), 2, false, false),
            "USB-C Expansion Card"
        );
    }

    #[test]
    fn usbc_that_was_sink_is_never_usba() {
        // Sink history must prevent misclassifying as USB-A even when currently stable Source+noPD.
        let p = port("Source", false, false, Some(7.5));
        let hist = [vec![port("Sink", false, false, None)]];
        assert_eq!(
            classify_pd_port(&p, hist.iter().map(|v| v.as_slice()), 2, false, true),
            "USB-C Expansion Card"
        );
    }

    #[test]
    fn stable_source_without_sink_history_is_usba() {
        let p = port("Source", false, false, Some(7.5));
        let hist = [
            vec![port("Source", false, false, Some(7.5))],
            vec![port("Source", false, false, Some(7.5))],
        ];
        assert_eq!(
            classify_pd_port(&p, hist.iter().map(|v| v.as_slice()), 2, false, false),
            "USB-A Expansion Card"
        );
    }

    /// Scripted mock behind `EcHw` so ordering/failover logic can be tested
    /// without hardware. Records every call in order; fallible calls fail
    /// while `fail_writes` is non-empty (decremented per failed call).
    pub struct MockEc {
        pub ops: std::sync::Mutex<Vec<String>>,
        pub fail_writes: std::sync::atomic::AtomicUsize,
        /// When set, `set_fan_duty` blocks until cleared: simulates a hung
        /// driver for dispatcher wedge/timeout tests.
        pub hang: std::sync::atomic::AtomicBool,
    }

    impl MockEc {
        pub fn new() -> Self {
            Self {
                ops: std::sync::Mutex::new(Vec::new()),
                fail_writes: std::sync::atomic::AtomicUsize::new(0),
                hang: std::sync::atomic::AtomicBool::new(false),
            }
        }

        pub fn with_hang(hang: bool) -> Self {
            let m = Self::new();
            m.set_hang(hang);
            m
        }

        pub fn set_hang(&self, hang: bool) {
            self.hang.store(hang, std::sync::atomic::Ordering::SeqCst);
        }

        fn record(&self, op: String) {
            self.ops.lock().unwrap().push(op);
        }

        fn fail_write(&self) -> Option<String> {
            // Decrement-if-nonzero via compare_exchange (stable since
            // forever); avoids `fetch_update`, which newer toolchains
            // renamed to `try_update` and deny under -D warnings.
            let mut left = self.fail_writes.load(std::sync::atomic::Ordering::SeqCst);
            loop {
                if left == 0 {
                    return None;
                }
                match self.fail_writes.compare_exchange_weak(
                    left,
                    left - 1,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                ) {
                    Ok(_) => return Some("mock write failure".to_string()),
                    Err(cur) => left = cur,
                }
            }
        }
    }

    impl EcHw for MockEc {
        fn thermal(&self) -> Result<ThermalData, String> {
            self.record("thermal".to_string());
            Ok(ThermalData {
                temps: Arc::new(BTreeMap::from([("CPU".to_string(), 50)])),
                fans: SmallVec::new(),
            })
        }

        fn power(&self) -> Result<BatteryData, String> {
            self.record("power".to_string());
            Ok(BatteryData::default())
        }

        fn versions(&self) -> Result<VersionsData, String> {
            self.record("versions".to_string());
            Ok(VersionsData::default())
        }

        fn set_fan_duty(&self, percent: u32, fan_index: Option<u32>) -> Result<(), String> {
            self.record(format!("set_fan_duty({percent},{fan_index:?})"));
            // Simulate a hung driver: block (briefly at a time) until the
            // test clears the flag. The dispatcher must time the caller
            // out instead of piling up blocked pool threads.
            while self.hang.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.fail_write().map_or(Ok(()), Err)
        }

        fn autofanctrl(&self) -> Result<(), String> {
            self.record("autofanctrl".to_string());
            self.fail_write().map_or(Ok(()), Err)
        }

        fn kblight_get(&self) -> Result<u32, String> {
            self.record("kblight_get".to_string());
            Ok(50)
        }

        fn kblight_set(&self, percent: u32) -> Result<(), String> {
            self.record(format!("kblight_set({percent})"));
            self.fail_write().map_or(Ok(()), Err)
        }

        fn fp_led_level_set(&self, level: &str) -> Result<(), String> {
            self.record(format!("fp_led_level_set({level})"));
            self.fail_write().map_or(Ok(()), Err)
        }

        fn charge_limit_set(&self, min_pct: u8, max_pct: u8) -> Result<(), String> {
            self.record(format!("charge_limit_set({min_pct},{max_pct})"));
            self.fail_write().map_or(Ok(()), Err)
        }

        fn pd_ports(&self) -> SmallVec<[UsbCPort; 4]> {
            self.record("pd_ports".to_string());
            SmallVec::new()
        }

        fn expansion_cards(&self) -> SmallVec<[ExpansionCard; 4]> {
            self.record("expansion_cards".to_string());
            SmallVec::new()
        }
    }

    #[test]
    fn mock_records_ops_in_call_order() {
        // Foundation for fence/queue tests: dispatch order through
        // `&dyn EcHw` must match the recorded hardware order.
        let mock = MockEc::new();
        let ec: &dyn EcHw = &mock;
        ec.autofanctrl().unwrap();
        ec.set_fan_duty(80, None).unwrap();
        ec.set_fan_duty(30, Some(0)).unwrap();
        assert_eq!(
            *mock.ops.lock().unwrap(),
            vec![
                "autofanctrl".to_string(),
                "set_fan_duty(80,None)".to_string(),
                "set_fan_duty(30,Some(0))".to_string(),
            ]
        );
    }

    #[test]
    fn mock_fails_scripted_writes() {
        let mock = MockEc::new();
        mock.fail_writes
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let ec: &dyn EcHw = &mock;
        assert!(ec.set_fan_duty(80, None).is_err());
        assert!(ec.autofanctrl().is_err());
        // Script exhausted: writes succeed again.
        assert!(ec.set_fan_duty(80, None).is_ok());
        assert_eq!(mock.ops.lock().unwrap().len(), 3);
    }
}
