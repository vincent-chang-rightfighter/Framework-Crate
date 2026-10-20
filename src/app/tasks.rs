use crate::cli;
use crate::types;
use crate::util;
use iced::Task;
use std::sync::Arc;
use tracing::warn;

use super::Message;

/// Quit path must not silently skip the firmware handover: one attempt
/// with a generous timeout instead of quick retries. Retries would only
/// re-queue duplicate handovers behind the same hung op, and a wedged
/// driver cannot recover inside the quit window anyway.
pub(crate) fn run_ec_task_quit(
    ec_client: &cli::SharedEc,
    done: Message,
    f: impl FnOnce(Arc<dyn cli::EcHw>) + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            const QUIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
            let ec_opt = { util::read_lock(&ec_client) };
            let Some(ref ec) = *ec_opt else {
                warn!("Quit EC handover skipped: no EC client (resume re-init?)");
                return done;
            };
            let ec = Arc::clone(ec);
            match crate::ec_serial::exec("quit-handover", QUIT_TIMEOUT, ec, f).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("Quit EC handover failed, fans may stay on manual duty: {e}");
                }
            }
            done
        },
        |msg| msg,
    )
}

/// Flushes persisted charge limit to EC on quit (config_save_task skips EC writes during shutdown).
pub(crate) fn apply_quit_charge_limit(ec: &dyn cli::EcHw, limit: &Option<types::SettingU8>) {
    if let Some(limit) = limit {
        let pct = if limit.enabled { limit.value } else { 100 };
        if let Err(e) = ec.charge_limit_set(0, pct) {
            warn!("Failed to set charge limit on quit: {}", e);
        }
    }
}

/// Runs a closure on the EC client and reports `Result` via `Message::EcOpResult` to surface EC write failures.
pub(crate) fn run_ec_task_result(
    ec_client: &cli::SharedEc,
    f: impl FnOnce(Arc<dyn cli::EcHw>) -> Result<(), String> + Send + 'static,
) -> Task<Message> {
    let ec_client = Arc::clone(ec_client);
    Task::perform(
        async move {
            let ec_opt = { util::read_lock(&ec_client) };
            let res: Result<(), String> = if let Some(ref ec) = *ec_opt {
                let ec = Arc::clone(ec);
                match crate::ec_serial::exec("ec-op", util::EC_IO_TIMEOUT, ec, f).await {
                    Ok(r) => r,
                    Err(e) => Err(e.to_string()),
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
