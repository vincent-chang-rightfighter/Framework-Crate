//! EC client acquisition for a poll round.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::warn;

use crate::app::AppState;
use crate::cli;
use crate::util::{read_lock, with_write_lock};

use super::super::{EC_INIT_SEM, try_ec_init_permit};

/// Ensures a live EC client, creating one under the init semaphore when
/// missing. Returns `None` when the round should be skipped (init not done,
/// init in flight, or creation failed).
pub(in crate::background_task) async fn phase_ensure_ec(
    state: &AppState,
) -> Option<Arc<dyn cli::EcHw>> {
    let ec_opt = { read_lock(&state.system.ec_client) };
    match ec_opt.as_ref().as_ref() {
        Some(c) => Some(Arc::clone(c)),
        None => {
            // Wait for init task before creating own client.
            if !state.system.ec_init_done.load(Ordering::Acquire) {
                return None;
            }
            let sem = EC_INIT_SEM.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)));
            let Some(permit) = try_ec_init_permit(sem) else {
                tracing::debug!("EC init already in flight; waiting");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                return None;
            };
            let state_cl = state.clone();
            match crate::util::spawn_blocking_with_timeout(crate::util::EC_IO_TIMEOUT, move || {
                let _permit = permit;
                cli::EcClient::new()
            })
            .await
            {
                Ok(Ok(c)) => {
                    let arc_ec: Arc<dyn cli::EcHw> = Arc::new(c);
                    with_write_lock(&state_cl.system.ec_client, |guard| {
                        *guard = Arc::new(Some(Arc::clone(&arc_ec)));
                    });
                    state_cl.system.cli_available.store(true, Ordering::Release);
                    Some(arc_ec)
                }
                Ok(Err(e)) => {
                    warn!("Background loop: failed to init EC: {}", e);
                    state_cl
                        .system
                        .cli_available
                        .store(false, Ordering::Release);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    None
                }
                Err(e) => {
                    warn!("Background loop: EC spawn_blocking panicked: {}", e);
                    state_cl
                        .system
                        .cli_available
                        .store(false, Ordering::Release);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    None
                }
            }
        }
    }
}
