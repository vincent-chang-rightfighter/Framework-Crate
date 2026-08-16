use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use parking_lot::RwLock;

use crate::cli;
use crate::temp_chart;
use crate::types::{Config, BatteryInfo};
use crate::util::{read_lock, with_write_lock};

/// Re-exported history type alias used by peripherals.
pub type PdPortsHistory = VecDeque<Arc<Vec<cli::ec_wrapper::UsbCPort>>>;

/// Fan control related state.
#[derive(Clone)]
pub struct FanState {
    /// Current fan control mode (raw u64 for atomic access).
    pub mode: Arc<AtomicU64>,
    /// Config poll interval (ms) for curve mode, synced from config.
    pub curve_poll_ms: Arc<AtomicU64>,
    /// Last duty cycle applied to the EC fan.
    pub last_applied_duty: Arc<AtomicU64>,
    /// Last known fan max RPM (periodically refreshed).
    pub fan_max_rpm: Arc<AtomicU64>,
    /// Timestamp of last fan_max_rpm reset.
    pub last_fan_rpm_reset: Arc<AtomicU64>,
    /// Full fan curve points (with zero/100 endpoints added).
    pub curve_full_points: Arc<RwLock<Arc<Vec<[u32; 2]>>>>,
    /// Number of fans detected (0 = unknown).
    pub fan_count: Arc<AtomicU64>,
    /// Whether to apply unified duty to all fans (true) or per-fan (false).
    pub unified_duty: Arc<AtomicBool>,
    /// Per-fan duty values (index = fan number).
    pub per_fan_duty: Arc<RwLock<Arc<Vec<u32>>>>,
}

impl Default for FanState {
    fn default() -> Self {
        Self {
            mode: Arc::new(AtomicU64::new(0)),
            curve_poll_ms: Arc::new(AtomicU64::new(1000)),
            last_applied_duty: Arc::new(AtomicU64::new(0)),
            fan_max_rpm: Arc::new(AtomicU64::new(0)),
            last_fan_rpm_reset: Arc::new(AtomicU64::new(0)),
            curve_full_points: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            fan_count: Arc::new(AtomicU64::new(0)),
            unified_duty: Arc::new(AtomicBool::new(true)),
            per_fan_duty: Arc::new(RwLock::new(Arc::new(Vec::new()))),
        }
    }
}

/// Thermal telemetry state.
#[derive(Clone)]
pub struct ThermalState {
    /// Latest thermal data from EC.
    pub data: Arc<RwLock<Arc<Option<cli::ec_wrapper::ThermalData>>>>,
    /// Temperature history for chart rendering.
    pub history: Arc<RwLock<Arc<temp_chart::ThermalHistory>>>,
    /// Sensor cache: sorted names, colors.
    pub sensor_cache: Arc<RwLock<Arc<crate::app::SensorCache>>>,
}

impl Default for ThermalState {
    fn default() -> Self {
        Self {
            data: Arc::new(RwLock::new(Arc::new(None))),
            history: Arc::new(RwLock::new(Arc::new(temp_chart::ThermalHistory::new()))),
            sensor_cache: Arc::new(RwLock::new(Arc::new(crate::app::SensorCache::default()))),
        }
    }
}

/// Read-only snapshot of thermal state for rendering.
pub struct ThermalSnapshot {
    pub data: Arc<Option<cli::ec_wrapper::ThermalData>>,
    pub sensor_cache: Arc<crate::app::SensorCache>,
    pub temp_history: Arc<std::collections::VecDeque<temp_chart::TempSample>>,
}

impl ThermalState {
    /// Take a consistent snapshot of all thermal fields.
    pub fn snapshot(&self, now_ms: i64) -> ThermalSnapshot {
        ThermalSnapshot {
            data: Arc::clone(&read_lock(&self.data)),
            sensor_cache: Arc::clone(&read_lock(&self.sensor_cache)),
            temp_history: with_write_lock(&self.history, |h| {
                Arc::make_mut(h).snapshot(now_ms)
            }),
        }
    }
}

/// Peripheral state (keyboard, expansion cards, USB-C ports).
#[derive(Clone)]
pub struct PeripheralState {
    /// Keyboard backlight level (0–100).
    pub kblight: Arc<RwLock<Arc<Option<u32>>>>,
    /// Detected expansion cards.
    pub expansion_cards: Arc<RwLock<Arc<Vec<cli::ec_wrapper::ExpansionCard>>>>,
    /// USB-C port state.
    pub pd_ports: Arc<RwLock<Arc<Vec<cli::ec_wrapper::UsbCPort>>>>,
    /// History of PD port snapshots (for stability classification).
    pub pd_ports_history: Arc<RwLock<Arc<PdPortsHistory>>>,
}

impl Default for PeripheralState {
    fn default() -> Self {
        Self {
            kblight: Arc::new(RwLock::new(Arc::new(None))),
            expansion_cards: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            pd_ports: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            pd_ports_history: Arc::new(RwLock::new(Arc::new(VecDeque::new()))),
        }
    }
}

/// Read-only snapshot of peripheral state for rendering.
pub struct PeripheralSnapshot {
    pub kblight: Arc<Option<u32>>,
    pub expansion_cards: Arc<Vec<cli::ec_wrapper::ExpansionCard>>,
    pub pd_ports: Arc<Vec<cli::ec_wrapper::UsbCPort>>,
    pub pd_ports_history: Arc<PdPortsHistory>,
}

impl PeripheralState {
    /// Take a consistent snapshot of all peripheral fields.
    pub fn snapshot(&self) -> PeripheralSnapshot {
        PeripheralSnapshot {
            kblight: Arc::clone(&read_lock(&self.kblight)),
            expansion_cards: Arc::clone(&read_lock(&self.expansion_cards)),
            pd_ports: Arc::clone(&read_lock(&self.pd_ports)),
            pd_ports_history: Arc::clone(&read_lock(&self.pd_ports_history)),
        }
    }
}

/// Battery state.
#[derive(Clone)]
pub struct BatteryState {
    /// Latest battery/power data.
    pub info: Arc<RwLock<Arc<Option<BatteryInfo>>>>,
    /// 上一次 AC 電源狀態（用於偵測 AC→Battery 轉換）
    pub prev_ac_present: Arc<AtomicBool>,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            info: Arc::new(RwLock::new(Arc::new(None))),
            prev_ac_present: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// System-level state (EC client, hardware info, sensor cache).
#[derive(Clone)]
pub struct SystemState {
    /// Whether the CLI/EC client is available.
    pub cli_available: Arc<AtomicBool>,
    /// The EC client instance.
    pub ec_client: Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    /// Firmware/hardware version data.
    pub versions: Arc<RwLock<Arc<Option<cli::ec_wrapper::VersionsData>>>>,
    /// Detected platform family (for feature gating).
    pub platform: Arc<RwLock<Arc<cli::ec_wrapper::PlatformFamily>>>,
}

impl Default for SystemState {
    fn default() -> Self {
        Self {
            cli_available: Arc::new(AtomicBool::new(false)),
            ec_client: Arc::new(RwLock::new(Arc::new(None))),
            versions: Arc::new(RwLock::new(Arc::new(None))),
            platform: Arc::new(RwLock::new(Arc::new(cli::ec_wrapper::PlatformFamily::Unknown))),
        }
    }
}

/// Lifecycle and UI coordination state.
#[derive(Clone)]
pub struct LifecycleState {
    /// Application configuration (shared with config_save_task).
    pub config: Arc<RwLock<Arc<Config>>>,
    /// Background telemetry poll interval (ms).
    pub poll_ms: Arc<AtomicU64>,
    /// Application shutdown flag.
    pub shutdown: Arc<AtomicBool>,
    /// Window visibility (tray minimize).
    pub visible: Arc<AtomicBool>,
    /// Last user interaction timestamp (for idle detection).
    pub last_interaction_ts: Arc<AtomicU64>,
    /// Background config save failure flag.
    pub bg_config_save_failed: Arc<AtomicBool>,
    /// View needs rebuild flag.
    pub view_dirty: Arc<AtomicBool>,
    /// Last system resume timestamp (ms since epoch). Set by the tray
    /// message pump when `WM_POWERBROADCAST` indicates a resume from
    /// sleep/hibernate. The background task watches this and resets the
    /// EC client and fan state when it changes.
    pub last_resume_ts: Arc<AtomicU64>,
    /// AC→battery transition detected: auto-reset PL1/PL2 on next tick.
    pub pl_reset_pending: Arc<AtomicBool>,
}

impl Default for LifecycleState {
    fn default() -> Self {
        Self {
            config: Arc::new(RwLock::new(Arc::new(Config::default()))),
            poll_ms: Arc::new(AtomicU64::new(500)),
            shutdown: Arc::new(AtomicBool::new(false)),
            visible: Arc::new(AtomicBool::new(true)),
            last_interaction_ts: Arc::new(AtomicU64::new(0)),
            bg_config_save_failed: Arc::new(AtomicBool::new(false)),
            view_dirty: Arc::new(AtomicBool::new(true)),
            last_resume_ts: Arc::new(AtomicU64::new(0)),
            pl_reset_pending: Arc::new(AtomicBool::new(false)),
        }
    }
}
