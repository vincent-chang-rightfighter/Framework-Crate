use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tracing::warn;

use crate::app::AppState;
use crate::util::read_lock;
use crate::background_task::pin_to_slowest_core;
use crate::types::{Config, SettingU8};

/// Debounce window: only save after this many ms of no further changes.
/// Prevents redundant disk writes during rapid slider drags.
const DEBOUNCE_MS: u64 = 100;

type BatteryKey = Option<SettingU8>;

fn battery_key(cfg: &Config) -> BatteryKey {
    cfg.battery.charge_limit_max_pct
}

async fn apply_battery_when_ready(cfg: &Config, state: &AppState) {
    for _ in 0..50 {
        if state.lifecycle.shutdown.load(Ordering::Acquire) {
            return;
        }
        {
            let ec = read_lock(&state.system.ec_client);
            if ec.as_ref().is_some() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    apply_battery_settings(cfg, state).await;
}

async fn apply_battery_settings(cfg: &Config, state: &AppState) {
    let ec = { read_lock(&state.system.ec_client) };
    let Some(ref ec) = *ec else { return };
    if let Some(ref limit) = cfg.battery.charge_limit_max_pct {
        let pct = if limit.enabled { limit.value } else { 100 };
        let ec_clone = ec.clone();
        // min_pct=0: Framework EC ignores the minimum charge limit parameter;
        // it only enforces the max. Using 0 here (not CHARGE_LIMIT_MIN=25)
        // because the EC's minimum is hardware-enforced at ~25%, and passing
        // 0 tells the EC "no software minimum" — the hardware minimum still applies.
        if let Err(e) = tokio::task::spawn_blocking(move || ec_clone.charge_limit_set(0, pct)).await.unwrap_or_else(|e| Err(format!("spawn error: {}", e))) {
            warn!("Failed to set charge limit: {}", e);
        }
    }
}

pub fn spawn(mut config_rx: watch::Receiver<Arc<Config>>, state: AppState) {
    tokio::spawn(async move {
        pin_to_slowest_core();
        let mut last_battery: Option<BatteryKey>;

        // Apply the persisted charge limit once EC is ready. The client is
        // initialized asynchronously, so retry until it appears or shutdown.
        {
            let cfg = Arc::clone(&config_rx.borrow());
            let key = battery_key(&cfg);
            last_battery = Some(key);
            if key.is_some() {
                apply_battery_when_ready(&cfg, &state).await;
            }
        }

        // Watch for changes — debounce: wait for the value to stabilise before saving.
        // This avoids redundant disk writes during rapid slider drags.
        loop {
            // Wait for the first change
            if config_rx.changed().await.is_err() { break; }

            // Drain rapid successive changes within the debounce window.
            // The timeout returns Err on timeout (normal), Ok(Err(_)) on channel close.
            let mut latest = Arc::clone(&config_rx.borrow());
            loop {
                match tokio::time::timeout(
                    Duration::from_millis(DEBOUNCE_MS),
                    config_rx.changed(),
                ).await {
                    Ok(Ok(())) => {
                        latest = Arc::clone(&config_rx.borrow());
                    }
                    Ok(Err(_)) => {
                        // Channel closed — save latest config and exit
                        let cfg_arc = Arc::clone(&latest);
                        let save_failed = Arc::clone(&state.lifecycle.bg_config_save_failed);
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = crate::config::save_fast(&cfg_arc) {
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
            let save_failed = Arc::clone(&state.lifecycle.bg_config_save_failed);
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::config::save_fast(&cfg_arc) {
                    warn!("Failed to save config: {}", e);
                    save_failed.store(true, Ordering::Relaxed);
                } else {
                    save_failed.store(false, Ordering::Relaxed);
                }
            }).await.unwrap_or_else(|e| warn!("config save task panicked: {}", e));

            let key = battery_key(&latest);
            if last_battery.as_ref() != Some(&key) {
                last_battery = Some(key);
                apply_battery_settings(&latest, &state).await;
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
