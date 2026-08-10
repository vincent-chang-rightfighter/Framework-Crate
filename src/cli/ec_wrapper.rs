use std::collections::BTreeMap;
use std::sync::Arc;
use framework_lib::chromium_ec::CrosEc;
use framework_lib::chromium_ec::CrosEcDriver;
use framework_lib::power;
use framework_lib::smbios;

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

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct ExpansionCard {
    pub name: String,
    pub active_firmware: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsbCPort {
    pub port: u32,
    pub pd_contract: bool,
    pub power_role: Option<String>,
    pub negotiated_text: Option<String>,
    pub negotiated_watts: Option<f32>,
    pub data_role: Option<String>,
    pub dp_alt_mode: bool,
}

pub struct EcClient {
    ec: CrosEc,
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

        if let Some(data) = self.ec.read_memory(0x00, 0x0F) {
            for (i, &byte) in data.iter().enumerate() {
                match byte {
                    0xFF => continue,
                    0xFE => continue,
                    0xFD => continue,
                    0xFC => continue,
                    _ => {
                        let temp = byte as i32 - 73;
                        let name = format!("Sensor {}", i);
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
            data.ec_build_version = Some(format!("RO:{} RW:{} Current:{:?}", ro, rw, current_image));
            data.ec_current_image = Some(format!("{:?}", current_image));
        }

        if let Some(esrt) = framework_lib::esrt::get_esrt() {
            for entry in esrt.entries.iter().take(esrt.resource_count as usize) {
                let version = format!(
                    "{:02X}.{:02X}.{:02X}.{:02X}",
                    entry.fw_version >> 24,
                    (entry.fw_version >> 16) & 0xFF,
                    (entry.fw_version >> 8) & 0xFF,
                    entry.fw_version & 0xFF
                );
                if entry.fw_type == 0 {
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

    pub fn charge_rate_limit_set(
        &self,
        rate_c: f32,
        soc_threshold_pct: Option<f32>,
    ) -> Result<(), String> {
        self.ec
            .set_charge_rate_limit(rate_c, soc_threshold_pct)
            .map_err(|e| format!("Failed to set charge rate limit: {:?}", e))
    }

    pub fn pd_ports(&self) -> Vec<UsbCPort> {
        let mut ports = Vec::new();
        let pd_infos = power::get_pd_info(&self.ec, 4);
        for (i, info) in pd_infos.into_iter().enumerate() {
            if let Ok(info) = info {
                let role = format!("{:?}", info.role);
                let data_role = if info.dualrole {
                    "Dual".to_string()
                } else {
                    "Source".to_string()
                };
                let negotiated_watts = if info.meas.voltage_max > 0 && info.meas.current_max > 0 {
                    Some(info.meas.voltage_max as f32 * info.meas.current_max as f32 / 1_000_000.0)
                } else {
                    None
                };
                let negotiated_text = negotiated_watts.map(|w| format!("{:.1}W", w));
                ports.push(UsbCPort {
                    port: i as u32,
                    pd_contract: info.charging_type != framework_lib::power::UsbChargingType::None,
                    power_role: Some(role),
                    negotiated_text,
                    negotiated_watts,
                    data_role: Some(data_role),
                    dp_alt_mode: false,
                });
            }
        }
        ports
    }

    pub fn expansion_cards(&self) -> Vec<ExpansionCard> {
        let mut cards = Vec::new();
        if let Ok(Some(board_id)) = self.ec.read_board_id_hc(
            framework_lib::chromium_ec::commands::BoardIdType::Mainboard,
        ) {
            if board_id != 0 {
                cards.push(ExpansionCard {
                    name: format!("Board ID: {:#06x}", board_id),
                    active_firmware: None,
                });
            }
        }
        cards
    }
}
