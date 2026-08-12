use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use parking_lot::RwLock;

use crate::cli;
use crate::temp_chart;
use crate::types::{Config, BatteryInfo};

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

/// Battery state.
#[derive(Clone)]
pub struct BatteryState {
    /// Latest battery/power data.
    pub info: Arc<RwLock<Arc<Option<BatteryInfo>>>>,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            info: Arc::new(RwLock::new(Arc::new(None))),
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
}

impl Default for SystemState {
    fn default() -> Self {
        Self {
            cli_available: Arc::new(AtomicBool::new(false)),
            ec_client: Arc::new(RwLock::new(Arc::new(None))),
            versions: Arc::new(RwLock::new(Arc::new(None))),
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
        }
    }
}
