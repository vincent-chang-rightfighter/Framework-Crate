use std::collections::BTreeMap;
use std::sync::Arc;
use framework_lib::chromium_ec::CrosEc;
use framework_lib::chromium_ec::CrosEcDriver;
use framework_lib::power;
use framework_lib::smbios;
use framework_lib::smbios::Platform;

#[derive(Debug, Clone)]
pub struct ThermalData {
    pub temps: Arc<BTreeMap<String, i32>>,
    pub fans: Vec<FanReading>,
}

#[derive(Debug, Clone)]
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
    pub tool_version: Option<String>,
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
    pub fn has_battery(self) -> bool { matches!(self, PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16) }
    pub fn has_fingerprint_led(self) -> bool { matches!(self, PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16) }
    pub fn has_keyboard_backlight(self) -> bool { matches!(self, PlatformFamily::Laptop12 | PlatformFamily::Laptop13 | PlatformFamily::Laptop16) }
}

pub fn detect_platform() -> PlatformFamily {
    match smbios::get_platform() {
        Some(Platform::Framework12IntelGen13) => PlatformFamily::Laptop12,
        Some(Platform::IntelCoreUltra1)
        | Some(Platform::IntelCoreUltra3)
        | Some(Platform::IntelGen11)
        | Some(Platform::IntelGen12)
        | Some(Platform::IntelGen13)
        | Some(Platform::Framework13Amd7080)
        | Some(Platform::Framework13AmdAi300) => PlatformFamily::Laptop13,
        Some(Platform::Framework16Amd7080)
        | Some(Platform::Framework16AmdAi300) => PlatformFamily::Laptop16,
        Some(Platform::FrameworkDesktopAmdAiMax300) => PlatformFamily::Desktop,
        _ => PlatformFamily::Unknown,
    }
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

// ============================================================================
// PD port classification helpers (ported from framework-control-windows-Iced)
// ============================================================================

const DISPLAY_CARD_WATTS_THRESHOLD: f32 = 3.0;

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

fn history_has_role(history: &[&Vec<UsbCPort>], port_id: u32, power_role: &str, pd_contract: bool) -> bool {
    history.iter().any(|h| h.iter().any(|p| p.port == port_id
        && p.power_role == Some(power_role)
        && p.pd_contract == pd_contract))
}

fn same_port_identity(a: &UsbCPort, b: &UsbCPort) -> bool {
    a.port == b.port
        && a.pd_contract == b.pd_contract
        && a.power_role == b.power_role
        && a.dp_alt_mode == b.dp_alt_mode
}

pub fn classify_pd_port<'a>(
    port: &UsbCPort,
    history: impl IntoIterator<Item = &'a Vec<UsbCPort>>,
    stable_threshold: usize,
    display_card_installed: bool,
) -> &'static str {
    /// Maximum number of history samples used for classification.
    /// Callers should pass exactly 3 samples (from `PdPortsHistory`); extras
    /// are silently ignored.
    const MAX_HIST: usize = 3;
    let empty = Vec::new();
    let mut hist_buf: [&Vec<UsbCPort>; MAX_HIST] = [&empty; MAX_HIST];
    let mut hist_len: usize = 0;
    for h in history {
        if hist_len < MAX_HIST {
            hist_buf[hist_len] = h;
            hist_len += 1;
        }
    }
    debug_assert!(
        hist_len <= MAX_HIST,
        "classify_pd_port received {} history samples, expected at most {}",
        hist_len,
        MAX_HIST
    );
    let history = &hist_buf[..hist_len];

    tracing::debug!(
        "[classify] Port {}: role={:?}, pd_contract={}, dp_alt={}, watts={:?}, hist_len={}, display_card={}",
        port.port, port.power_role, port.pd_contract, port.dp_alt_mode, port.negotiated_watts, hist_len, display_card_installed
    );

    if is_pd_power_input(port) {
        tracing::debug!("[classify] Port {} → USB-C Expansion Card (Sink+PD)", port.port);
        return "USB-C Expansion Card";
    }
    if port.dp_alt_mode {
        tracing::debug!("[classify] Port {} → DP/HDMI Expansion Card (dp_alt)", port.port);
        return "DP/HDMI Expansion Card";
    }
    if port.pd_contract && role_is(port, "Source") {
        if port.dp_alt_mode {
            let result = match port.negotiated_watts {
                Some(w) if w <= DISPLAY_CARD_WATTS_THRESHOLD => "DisplayPort Expansion Card",
                Some(_) => "HDMI Expansion Card",
                None => "DP/HDMI Expansion Card",
            };
            tracing::debug!("[classify] Port {} → {} (Source+PD+dp_alt, watts={:?})", port.port, result, port.negotiated_watts);
            return result;
        }
        tracing::debug!("[classify] Port {} → USB-C Expansion Card (Source+PD, no dp_alt, watts={:?})", port.port, port.negotiated_watts);
        return "USB-C Expansion Card";
    }
    if is_source_no_pd(port) {
        let has_seen_sink = history_has_role(history, port.port, "Sink", false)
            || history_has_role(history, port.port, "Sink", true);
        if has_seen_sink {
            tracing::debug!("[classify] Port {} → USB-C Expansion Card (Source+noPD, seen Sink)", port.port);
            return "USB-C Expansion Card";
        }
        let stable_count = history.iter()
            .filter(|h| h.iter().any(|p| same_port_identity(p, port)))
            .count();
        tracing::debug!("[classify] Port {} Source+noPD: stable_count={}, threshold={}", port.port, stable_count, stable_threshold);
        if stable_count >= stable_threshold {
            tracing::debug!("[classify] Port {} → USB-A Expansion Card (stable Source)", port.port);
            return "USB-A Expansion Card";
        }
        tracing::debug!("[classify] Port {} → USB-C Expansion Card (Source+noPD, pending USB-A check)", port.port);
        return "USB-C Expansion Card";
    }
    if is_sink_no_pd(port) {
        tracing::debug!("[classify] Port {} → USB-C Expansion Card (Sink+noPD)", port.port);
        return "USB-C Expansion Card";
    }
    tracing::debug!("[classify] Port {} → USB-C Port (fallback)", port.port);
    "USB-C Port"
}

pub struct EcClient {
    ec: CrosEc,
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
                5 if matches!(platform, Some(Platform::IntelGen12) | Some(Platform::IntelGen13)) => "F57397_VCCGT",
                _ => return format!("Sensor {}", index),
            }
        }
        Some(Platform::IntelCoreUltra1) | Some(Platform::IntelCoreUltra3) => {
            match index {
                0 => "F75303_Local",
                1 => "F75303_CPU",
                2 => "Battery",
                3 => "F75303_DDR",
                4 => "PECI",
                _ => return format!("Sensor {}", index),
            }
        }
        Some(Platform::Framework12IntelGen13) => {
            match index {
                0 => "F75303_CPU",
                1 => "F75303_Skin",
                2 => "F75303_Local",
                3 => "Battery",
                4 => "PECI",
                5 => "Charger IC",
                _ => return format!("Sensor {}", index),
            }
        }
        Some(
            Platform::Framework13Amd7080
            | Platform::Framework13AmdAi300
            | Platform::Framework16Amd7080
            | Platform::Framework16AmdAi300,
        ) => {
            let is_16 = matches!(platform, Some(Platform::Framework16Amd7080) | Some(Platform::Framework16AmdAi300));
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
        Some(Platform::FrameworkDesktopAmdAiMax300) => {
            match index {
                0 => "F75303_APU",
                1 => "F75303_DDR",
                2 => "F75303_AMB",
                3 => "APU",
                4 => "Virtual",
                _ => return format!("Sensor {}", index),
            }
        }
        _ => return format!("Sensor {}", index),
    };
    name.to_string()
}

/// Per-platform sensor name tables are immutable once resolved. `thermal()`
/// polls every cycle, so cache the names (keyed by platform) and only clone
/// the one String needed per sensor instead of re-matching + re-allocating.
/// Cache of per-platform sensor name tables (platform, names).
type SensorNamesCache = std::sync::Mutex<Option<(Option<Platform>, Vec<String>)>>;

static SENSOR_NAMES: std::sync::OnceLock<SensorNamesCache> = std::sync::OnceLock::new();

fn sensor_name(platform: Option<Platform>, index: usize) -> String {
    let cache = SENSOR_NAMES.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
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

    pub fn thermal(&self) -> Result<ThermalData, String> {
        let mut temps = BTreeMap::new();
        let mut fans = Vec::new();

        let platform = smbios::get_platform();

        if let Some(data) = self.ec.read_memory(0x00, 0x0F) {
            for (i, &byte) in data.iter().enumerate() {
                match byte {
                    0xFC..=0xFF => continue,
                    _ => {
                        let temp = byte as i32 - 73;
                        let name = sensor_name(platform, i);
                        temps.insert(name, temp);
                    }
                }
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

        Ok(ThermalData { temps: Arc::new(temps), fans })
    }

    pub fn power(&self) -> Result<BatteryData, String> {
        let mut data = BatteryData::default();

        let info = power::power_info(&self.ec)
            .ok_or_else(|| "power_info returned None (no battery data available)".to_string())?;
        data.ac_present = Some(info.ac_present);
        if let Some(ref batt) = info.battery {
            data.present_voltage_mv = Some(batt.present_voltage);
            data.present_rate_ma = Some(batt.present_rate);
            data.remaining_capacity_mah = Some(batt.remaining_capacity);
            data.design_capacity_mah = Some(batt.design_capacity);
            data.design_capacity_wh = Some(
                batt.design_capacity as f32 * batt.design_voltage as f32 / 1_000_000.0,
            );
            data.last_full_charge_capacity_mah = Some(batt.last_full_charge_capacity);
            data.cycle_count = Some(batt.cycle_count);
            data.soc_pct = Some(batt.charge_percentage);
            data.discharging = Some(batt.discharging);
            data.manufacturer = Some(batt.manufacturer.clone());
            data.model_number = Some(batt.model_number.clone());
            data.serial_number = Some(batt.serial_number.clone());
            data.battery_type = Some(batt.battery_type.clone());

            if batt.design_voltage > 0 {
                data.remaining_capacity_wh = Some(
                    batt.remaining_capacity as f32 * batt.design_voltage as f32
                        / 1_000_000.0,
                );
            }
        }

        Ok(data)
    }

    pub fn versions(&self) -> Result<VersionsData, String> {
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
                if entry.fw_type == 1 {
                    data.uefi_version = Some(version);
                }
            }
        }

        Ok(data)
    }

    pub fn set_fan_duty(&self, percent: u32, fan_index: Option<u32>) -> Result<(), String> {
        self.ec
            .fan_set_duty(fan_index, percent)
            .map_err(|e| format!("Failed to set fan duty: {:?}", e))
    }

    pub fn autofanctrl(&self) -> Result<(), String> {
        self.ec
            .autofanctrl(None)
            .map_err(|e| format!("Failed to set auto fan control: {:?}", e))
    }

    pub fn kblight_get(&self) -> Result<u32, String> {
        self.ec
            .get_keyboard_backlight()
            .map(|v| v as u32)
            .map_err(|e| format!("Failed to get keyboard backlight: {:?}", e))
    }

    pub fn kblight_set(&self, percent: u32) -> Result<(), String> {
        self.ec.set_keyboard_backlight(percent as u8);
        Ok(())
    }

    pub fn fp_led_level_set(&self, level: &str) -> Result<(), String> {
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

    pub fn charge_limit_set(&self, min_pct: u8, max_pct: u8) -> Result<(), String> {
        self.ec
            .set_charge_limit(min_pct, max_pct)
            .map_err(|e| format!("Failed to set charge limit: {:?}", e))
    }

    #[allow(dead_code)]
    pub fn get_charge_limit(&self) -> Result<(u8, u8), String> {
        self.ec
            .get_charge_limit()
            .map_err(|e| format!("Failed to get charge limit: {:?}", e))
    }

    pub fn pd_ports(&self) -> Vec<UsbCPort> {
        use framework_lib::chromium_ec::commands::EcRequestGetPdPortState;
        use framework_lib::chromium_ec::EcRequestRaw;

        let mut ports = Vec::new();
        for i in 0u8..4 {
            let info = match (EcRequestGetPdPortState { port: i }).send_command(&self.ec) {
                Ok(info) => info,
                Err(_) => continue,
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
            let watts_mw = voltage as u32 * current as u32 / 1000;
            let negotiated_watts = if watts_mw > 0 {
                Some(watts_mw as f32 / 1000.0)
            } else {
                None
            };
            let negotiated_text = if voltage > 0 && current > 0 {
                Some(format!("PD {:.0}W, {:.0}V, {:.2}A", watts_mw as f32 / 1000.0, voltage as f32 / 1000.0, current as f32 / 1000.0))
            } else {
                None
            };

            tracing::debug!(
                "[pd_ports] Port {}: c_state={}, pd_state={}, role={}, data_role={}, v={}mV i={}mA dp_alt=0x{:02X} watts={:?}",
                i, c_state, pd_state, power_role, data_role, voltage, current, dp_alt_raw, negotiated_watts
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

    pub fn expansion_cards(&self) -> Vec<ExpansionCard> {
        let mut cards = Vec::new();
        if let Ok(Some(board_id)) = self.ec.read_board_id_hc(
            framework_lib::chromium_ec::commands::BoardIdType::Mainboard,
        )
            && board_id != 0
        {
            cards.push(ExpansionCard {
                name: format!("Board ID: {:#06x}", board_id),
                active_firmware: None,
            });
        }
        cards
    }
}
