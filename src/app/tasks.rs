use crate::cli;
use crate::types;
use crate::util;
use iced::Task;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::warn;

use super::Message;

/// Executes closure on EC client via spawn_blocking; no-ops silently if unavailable.
pub(crate) fn run_ec_task(
    ec_client: &Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    done: Message,
    f: impl FnOnce(Arc<cli::EcClient>) + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let _guard = util::acquire_ec_write().await;
            let ec_opt = { util::read_lock(&ec_client) };
            if let Some(ref ec) = *ec_opt {
                let ec = ec.clone();
                if let Err(e) =
                    util::spawn_blocking_with_timeout(util::EC_IO_TIMEOUT, move || f(ec)).await
                {
                    warn!("EC task failed: {}", e);
                }
            }
            done
        },
        |msg| msg,
    )
}

/// Flushes persisted charge limit to EC on quit (config_save_task skips EC writes during shutdown).
pub(crate) fn apply_quit_charge_limit(ec: &cli::EcClient, limit: &Option<types::SettingU8>) {
    if let Some(limit) = limit {
        let pct = if limit.enabled { limit.value } else { 100 };
        if let Err(e) = ec.charge_limit_set(0, pct) {
            warn!("Failed to set charge limit on quit: {}", e);
        }
    }
}

/// Like `run_ec_task` but reports `Result` via `Message::EcOpResult` to surface EC write failures.
pub(crate) fn run_ec_task_result(
    ec_client: &Arc<RwLock<Arc<Option<Arc<cli::EcClient>>>>>,
    f: impl FnOnce(Arc<cli::EcClient>) -> Result<(), String> + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let _guard = util::acquire_ec_write().await;
            let ec_opt = { util::read_lock(&ec_client) };
            let res = if let Some(ref ec) = *ec_opt {
                let ec = ec.clone();
                match util::spawn_blocking_with_timeout(util::EC_IO_TIMEOUT, move || f(ec)).await {
                    Ok(r) => r,
                    Err(e) => Err(e),
                }
            } else {
                // No EC client: silently no-op to avoid spurious error.
                Ok(())
            };
            Message::EcOpResult(res.err())
        },
        |msg| msg,
    )
}

/// Refreshes CPU power data off UI thread; `after` runs after refresh and completion sends `Message::CpuPowerDataRefreshed`.
pub(crate) fn refresh_cpu_power_task(
    state: crate::cpu_power::CpuPowerState,
    after: impl FnOnce() + Send + 'static,
) -> Task<Message> {
    let task_state = state.clone();
    Task::perform(
        async move {
            let _ = util::spawn_blocking_with_timeout(util::PAWNIO_IO_TIMEOUT, move || {
                task_state.refresh();
                after();
            })
            .await;
            Message::CpuPowerDataRefreshed
        },
        |msg| msg,
    )
}

/// Stops CPU power sync thread off UI thread (join may block up to 250ms).
pub(crate) fn stop_sync_task(state: crate::cpu_power::CpuPowerState) -> Task<Message> {
    Task::perform(
        async move {
            let _guard = util::cpu_power_mutex().lock().await;
            let _ = util::spawn_blocking_with_timeout(util::PAWNIO_IO_TIMEOUT, move || {
                state.stop_sync()
            })
            .await;
            Message::CpuPowerSyncStopped
        },
        |msg| msg,
    )
}

/// Self-rescheduling UI tick via tokio::time to avoid busy polling.
pub(crate) fn tick_task(ms: u64) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        },
        |_| Message::Tick,
    )
}
