use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tracing::warn;

use crate::app::{AppState, read_lock};
use crate::background_task::pin_to_slowest_core;
use crate::types::{Config, SettingU8};

/// Debounce window: only save after this many ms of no further changes.
/// Prevents redundant disk writes during rapid slider drags.
const DEBOUNCE_MS: u64 = 100;

type BatteryKey = Option<SettingU8>;

fn battery_key(cfg: &Config) -> BatteryKey {
    cfg.battery.charge_limit_max_pct
}

async fn apply_battery_settings(cfg: &Config, state: &AppState) {
    let ec = { read_lock(&state.ec_client) };
    let Some(ref ec) = *ec else { return };
    if let Some(ref limit) = cfg.battery.charge_limit_max_pct {
        let pct = if limit.enabled { limit.value } else { 100 };
        let ec_clone = ec.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || ec_clone.charge_limit_set(0, pct)).await.unwrap_or_else(|e| Err(format!("spawn error: {}", e))) {
            warn!("Failed to set charge limit: {}", e);
        }
    }
}

pub fn spawn(mut config_rx: watch::Receiver<Arc<Config>>, state: AppState) {
    tokio::spawn(async move {
        pin_to_slowest_core();
        let mut last_battery: Option<BatteryKey>;

        // Process initial value
        {
            let cfg = config_rx.borrow().clone();
            let cfg_arc = Arc::clone(&cfg);
            let state_clone = state.clone();
            let save_failed = Arc::clone(&state_clone.bg_config_save_failed);
            let save_ok = match tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::config::save(&cfg_arc) {
                    warn!("Failed to save config: {}", e);
                    save_failed.store(true, Ordering::Relaxed);
                    false
                } else {
                    save_failed.store(false, Ordering::Relaxed);
                    true
                }
            }).await {
                Ok(ok) => ok,
                Err(e) => {
                    warn!("config save task panicked: {}", e);
                    false
                }
            };

            let key = battery_key(&cfg);
            last_battery = Some(key);
            if save_ok {
                apply_battery_settings(&cfg, &state_clone).await;
            }
        }

        // Watch for changes — debounce: wait for the value to stabilise before saving.
        // This avoids redundant disk writes during rapid slider drags.
        loop {
            // Wait for the first change
            if config_rx.changed().await.is_err() { break; }

            // Drain rapid successive changes within the debounce window.
            // The timeout returns Err on timeout (normal), Ok(Err(_)) on channel close.
            let mut latest = config_rx.borrow().clone();
            loop {
                match tokio::time::timeout(
                    Duration::from_millis(DEBOUNCE_MS),
                    config_rx.changed(),
                ).await {
                    Ok(Ok(())) => {
                        latest = config_rx.borrow().clone();
                    }
                    Ok(Err(_)) => {
                        // Channel closed — save latest config and exit
                        let cfg_arc = Arc::clone(&latest);
                        let state_clone = state.clone();
                        let save_failed = Arc::clone(&state_clone.bg_config_save_failed);
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = crate::config::save(&cfg_arc) {
                                warn!("Failed to save config on channel close: {}", e);
                                save_failed.store(true, Ordering::Relaxed);
                            }
                        }).await.unwrap_or_else(|e| warn!("config save task panicked: {}", e));
                        return;
                    }
                    Err(_) => {
                        // Debounce timeout — save the latest value
                        break;
                    }
                }
            }

            let cfg_arc = Arc::clone(&latest);
            let state_clone = state.clone();
            let save_failed = Arc::clone(&state_clone.bg_config_save_failed);
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::config::save(&cfg_arc) {
                    warn!("Failed to save config: {}", e);
                    save_failed.store(true, Ordering::Relaxed);
                } else {
                    save_failed.store(false, Ordering::Relaxed);
                }
            }).await.unwrap_or_else(|e| warn!("config save task panicked: {}", e));

            let key = battery_key(&latest);
            if last_battery.as_ref() != Some(&key) {
                last_battery = Some(key);
                apply_battery_settings(&latest, &state_clone).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn battery_key_default() {
        let cfg = default_config();
        let key = battery_key(&cfg);
        assert_eq!(key, None);
    }

    #[test]
    fn battery_key_with_limit() {
        let mut cfg = default_config();
        cfg.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 80 });
        let key = battery_key(&cfg);
        assert_eq!(key, Some(SettingU8 { enabled: true, value: 80 }));
    }

    #[test]
    fn battery_key_equal_for_same_config() {
        let mut cfg1 = default_config();
        cfg1.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 75 });

        let mut cfg2 = default_config();
        cfg2.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 75 });

        assert_eq!(battery_key(&cfg1), battery_key(&cfg2));
    }

    #[test]
    fn battery_key_different_when_limit_differs() {
        let mut cfg1 = default_config();
        cfg1.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 75 });

        let mut cfg2 = default_config();
        cfg2.battery.charge_limit_max_pct = Some(SettingU8 { enabled: true, value: 80 });

        assert_ne!(battery_key(&cfg1), battery_key(&cfg2));
    }
}
