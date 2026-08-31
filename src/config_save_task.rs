use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tracing::warn;

use crate::app::AppState;
use crate::background_task::pin_to_slowest_core;
use crate::types::{Config, SettingU8};
use crate::util::read_lock;

/// Debounce window to coalesce rapid slider changes.
const DEBOUNCE_MS: u64 = 100;

type BatteryKey = Option<SettingU8>;

fn battery_key(cfg: &Config) -> BatteryKey {
    cfg.battery.charge_limit_max_pct
}

async fn apply_battery_when_ready(cfg: &Config, state: &AppState) -> bool {
    for _ in 0..50 {
        if state.lifecycle.shutdown.load(Ordering::Acquire) {
            return false;
        }
        {
            let ec = read_lock(&state.system.ec_client);
            if ec.as_ref().is_some() {
                return apply_battery_settings(cfg, state).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn apply_battery_settings(cfg: &Config, state: &AppState) -> bool {
    // Skip EC writes during shutdown.
    if state.lifecycle.shutdown.load(Ordering::Acquire) {
        return false;
    }
    let ec = { read_lock(&state.system.ec_client) };
    let Some(ref ec) = *ec else { return false };
    if let Some(ref limit) = cfg.battery.charge_limit_max_pct {
        let pct = if limit.enabled { limit.value } else { 100 };
        let ec_clone = ec.clone();
        let _guard = crate::util::acquire_ec_write().await;
        // min_pct=0: EC ignores software minimum; hardware enforces ~25%.
        if let Err(e) =
            crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                ec_clone.charge_limit_set(0, pct)
            })
            .await
            .unwrap_or_else(Err)
        {
            warn!("Failed to set charge limit: {}", e);
            return false;
        }
    }
    true
}

pub fn spawn(mut config_rx: watch::Receiver<(Arc<Config>, u64)>, state: AppState) {
    tokio::spawn(async move {
        // NOTE: Pins calling thread; acceptable for lightweight long-lived task.
        pin_to_slowest_core();
        let mut last_battery: Option<BatteryKey>;

        // Apply persisted charge limit once EC is ready; retry until available or shutdown.
        {
            let (cfg, _ver) = config_rx.borrow().clone();
            let key = battery_key(&cfg);
            last_battery = Some(key);
            // Keep out of sync if apply failed so next change retries.
            if key.is_some() && !apply_battery_when_ready(&cfg, &state).await {
                last_battery = None;
            }
        }

        // Debounce changes to avoid redundant disk writes.
        loop {
            // Retry battery apply periodically if still pending; do not block forever.
            let changed = if last_battery.is_none() {
                match tokio::time::timeout(Duration::from_secs(5), config_rx.changed()).await {
                    Ok(r) => r,
                    Err(_) => {
                        let (cfg, _ver) = config_rx.borrow().clone();
                        if apply_battery_settings(&cfg, &state).await {
                            last_battery = Some(battery_key(&cfg));
                        }
                        continue;
                    }
                }
            } else {
                config_rx.changed().await
            };
            if changed.is_err() {
                break;
            }

            // Drain rapid changes within debounce window.
            let mut latest = config_rx.borrow().clone();
            loop {
                match tokio::time::timeout(Duration::from_millis(DEBOUNCE_MS), config_rx.changed())
                    .await
                {
                    Ok(Ok(())) => {
                        latest = config_rx.borrow().clone();
                    }
                    Ok(Err(_)) => {
                        // Channel closed; save latest and exit (versioned).
                        let (cfg_arc, ver) = latest;
                        let save_failed = Arc::clone(&state.lifecycle.bg_config_save_failed);
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::EC_IO_TIMEOUT,
                            move || {
                                if let Err(e) = crate::config::save_versioned(&cfg_arc, ver, false)
                                {
                                    warn!("Failed to save config on channel close: {}", e);
                                    save_failed.store(true, Ordering::Relaxed);
                                }
                            },
                        )
                        .await
                        .unwrap_or_else(|e| warn!("config save task panicked: {}", e));
                        return;
                    }
                    Err(_) => {
                        // Debounce timeout — save the latest value
                        break;
                    }
                }
            }

            // Versioned: newer shutdown save supersedes this older snapshot.
            let (cfg_arc, ver) = latest;
            let cfg_for_battery = Arc::clone(&cfg_arc);
            let save_failed = Arc::clone(&state.lifecycle.bg_config_save_failed);
            crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                if let Err(e) = crate::config::save_versioned(&cfg_arc, ver, false) {
                    warn!("Failed to save config: {}", e);
                    save_failed.store(true, Ordering::Relaxed);
                } else {
                    save_failed.store(false, Ordering::Relaxed);
                }
            })
            .await
            .unwrap_or_else(|e| warn!("config save task panicked: {}", e));

            let key = battery_key(&cfg_for_battery);
            if last_battery.as_ref() != Some(&key) {
                // Retry on failure; only advance on success.
                if apply_battery_settings(&cfg_for_battery, &state).await {
                    last_battery = Some(key);
                }
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
        cfg.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 80,
        });
        let key = battery_key(&cfg);
        assert_eq!(
            key,
            Some(SettingU8 {
                enabled: true,
                value: 80
            })
        );
    }

    #[test]
    fn battery_key_equal_for_same_config() {
        let mut cfg1 = default_config();
        cfg1.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 75,
        });

        let mut cfg2 = default_config();
        cfg2.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 75,
        });

        assert_eq!(battery_key(&cfg1), battery_key(&cfg2));
    }

    #[test]
    fn battery_key_different_when_limit_differs() {
        let mut cfg1 = default_config();
        cfg1.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 75,
        });

        let mut cfg2 = default_config();
        cfg2.battery.charge_limit_max_pct = Some(SettingU8 {
            enabled: true,
            value: 80,
        });

        assert_ne!(battery_key(&cfg1), battery_key(&cfg2));
    }
}
