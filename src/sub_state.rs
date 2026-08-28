use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use parking_lot::RwLock;
use smallvec::SmallVec;

use crate::cli;
use crate::temp_chart;
use crate::types::{BatteryInfo, Config};
use crate::util::{read_lock, with_write_lock};

pub type PdPortsHistory = VecDeque<Arc<SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>>;

#[derive(Clone)]
pub struct FanState {
    pub mode: Arc<AtomicU64>,
    pub last_applied_duty: Arc<AtomicU64>,
    pub fan_max_rpm: Arc<AtomicU64>,
    pub last_fan_rpm_reset: Arc<AtomicU64>,
    pub curve_full_points: Arc<RwLock<Arc<Vec<[u32; 2]>>>>,
    pub fan_count: Arc<AtomicU64>,
    pub unified_duty: Arc<AtomicBool>,
    pub per_fan_duty: Arc<RwLock<Arc<Vec<u32>>>>,
    pub last_fan_count: Arc<AtomicU64>,
}

impl Default for FanState {
    fn default() -> Self {
        Self {
            mode: Arc::new(AtomicU64::new(0)),
            last_applied_duty: Arc::new(AtomicU64::new(0)),
            fan_max_rpm: Arc::new(AtomicU64::new(0)),
            last_fan_rpm_reset: Arc::new(AtomicU64::new(0)),
            curve_full_points: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            fan_count: Arc::new(AtomicU64::new(0)),
            unified_duty: Arc::new(AtomicBool::new(true)),
            per_fan_duty: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            last_fan_count: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Clone)]
pub struct ThermalState {
    pub data: Arc<RwLock<Arc<Option<cli::ec_wrapper::ThermalData>>>>,
    pub history: Arc<RwLock<Arc<temp_chart::ThermalHistory>>>,
    pub sensor_cache: Arc<RwLock<Arc<crate::app::SensorCache>>>,
    pub last_success_ms: Arc<AtomicU64>,
}

impl Default for ThermalState {
    fn default() -> Self {
        Self {
            data: Arc::new(RwLock::new(Arc::new(None))),
            history: Arc::new(RwLock::new(Arc::new(temp_chart::ThermalHistory::new()))),
            sensor_cache: Arc::new(RwLock::new(Arc::new(crate::app::SensorCache::default()))),
            last_success_ms: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub struct ThermalSnapshot {
    pub data: Arc<Option<cli::ec_wrapper::ThermalData>>,
    pub sensor_cache: Arc<crate::app::SensorCache>,
    pub temp_history: Arc<std::collections::VecDeque<temp_chart::TempSample>>,
}

impl ThermalState {
    pub fn snapshot(&self, now_ms: i64) -> ThermalSnapshot {
        ThermalSnapshot {
            data: Arc::clone(&read_lock(&self.data)),
            sensor_cache: Arc::clone(&read_lock(&self.sensor_cache)),
            temp_history: with_write_lock(&self.history, |h| Arc::make_mut(h).snapshot(now_ms)),
        }
    }
}

#[derive(Clone)]
pub struct PeripheralState {
    pub kblight: Arc<RwLock<Arc<Option<u32>>>>,
    pub expansion_cards: Arc<RwLock<Arc<SmallVec<[cli::ec_wrapper::ExpansionCard; 4]>>>>,
    pub pd_ports: Arc<RwLock<Arc<SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>>>,
    pub pd_ports_history: Arc<RwLock<Arc<PdPortsHistory>>>,
    /// NOTE: Ports once seen as Sink are permanently USB-C.
    pub pd_usb_c_seen: Arc<RwLock<Arc<Vec<bool>>>>,
}

impl Default for PeripheralState {
    fn default() -> Self {
        Self {
            kblight: Arc::new(RwLock::new(Arc::new(None))),
            expansion_cards: Arc::new(RwLock::new(Arc::new(SmallVec::new()))),
            pd_ports: Arc::new(RwLock::new(Arc::new(SmallVec::new()))),
            pd_ports_history: Arc::new(RwLock::new(Arc::new(VecDeque::new()))),
            pd_usb_c_seen: Arc::new(RwLock::new(Arc::new(Vec::new()))),
        }
    }
}

pub struct PeripheralSnapshot {
    pub kblight: Arc<Option<u32>>,
    pub expansion_cards: Arc<SmallVec<[cli::ec_wrapper::ExpansionCard; 4]>>,
    pub pd_ports: Arc<SmallVec<[cli::ec_wrapper::UsbCPort; 4]>>,
    pub pd_ports_history: Arc<PdPortsHistory>,
    pub pd_usb_c_seen: Arc<Vec<bool>>,
}

impl PeripheralState {
    pub fn snapshot(&self) -> PeripheralSnapshot {
        PeripheralSnapshot {
            kblight: Arc::clone(&read_lock(&self.kblight)),
            expansion_cards: Arc::clone(&read_lock(&self.expansion_cards)),
            pd_ports: Arc::clone(&read_lock(&self.pd_ports)),
            pd_ports_history: Arc::clone(&read_lock(&self.pd_ports_history)),
            pd_usb_c_seen: Arc::clone(&read_lock(&self.pd_usb_c_seen)),
        }
    }
}

#[derive(Clone)]
pub struct BatteryState {
    pub info: Arc<RwLock<Arc<Option<BatteryInfo>>>>,
    /// NOTE: Tracks AC→battery transitions.
    pub prev_ac_present: Arc<AtomicBool>,
    pub last_success_ms: Arc<AtomicU64>,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            info: Arc::new(RwLock::new(Arc::new(None))),
            prev_ac_present: Arc::new(AtomicBool::new(true)),
            last_success_ms: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Clone)]
pub struct SystemState {
    pub cli_available: Arc<AtomicBool>,
    pub ec_client: Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    /// NOTE: While false, background loop must not create EC client concurrently.
    pub ec_init_done: Arc<AtomicBool>,
    pub versions: Arc<RwLock<Arc<Option<cli::ec_wrapper::VersionsData>>>>,
    pub platform: Arc<RwLock<Arc<cli::ec_wrapper::PlatformFamily>>>,
    pub intel_cpu: Arc<AtomicBool>,
}

impl Default for SystemState {
    fn default() -> Self {
        Self {
            cli_available: Arc::new(AtomicBool::new(false)),
            ec_client: Arc::new(RwLock::new(Arc::new(None))),
            ec_init_done: Arc::new(AtomicBool::new(false)),
            versions: Arc::new(RwLock::new(Arc::new(None))),
            platform: Arc::new(RwLock::new(Arc::new(
                cli::ec_wrapper::PlatformFamily::Unknown,
            ))),
            intel_cpu: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone)]
pub struct LifecycleState {
    pub config: Arc<RwLock<Arc<Config>>>,
    pub poll_ms: Arc<AtomicU64>,
    pub shutdown: Arc<AtomicBool>,
    pub visible: Arc<AtomicBool>,
    pub last_interaction_ts: Arc<AtomicU64>,
    pub bg_config_save_failed: Arc<AtomicBool>,
    pub view_dirty: Arc<AtomicBool>,
    pub view_generation: Arc<AtomicU64>,
    /// NOTE: Set on WM_POWERBROADCAST resume; triggers EC client reset.
    pub last_resume_ts: Arc<AtomicU64>,
    pub pl_reset_pending: Arc<AtomicBool>,
    pub fan_reset_pending: Arc<AtomicBool>,
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
            view_generation: Arc::new(AtomicU64::new(1)),
            last_resume_ts: Arc::new(AtomicU64::new(0)),
            pl_reset_pending: Arc::new(AtomicBool::new(false)),
            fan_reset_pending: Arc::new(AtomicBool::new(false)),
        }
    }
}
