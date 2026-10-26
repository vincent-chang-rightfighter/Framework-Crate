use super::{App, Message, PL_RESET_RETRY_MS, refresh_cpu_power_task, stop_sync_task};
use iced::Task;
use std::sync::atomic::Ordering;
use tracing::{error, warn};

fn sync_restart_params(
    was_sync: bool,
    write_ok: bool,
    applied: crate::cpu_power::PowerLimitParams,
    previous: Option<crate::cpu_power::PowerLimitParams>,
) -> Option<crate::cpu_power::PowerLimitParams> {
    if !was_sync {
        return None;
    }
    if write_ok { Some(applied) } else { previous }
}

fn sync_restart_after_reset(
    was_sync: bool,
    reset_ok: bool,
    previous: Option<crate::cpu_power::PowerLimitParams>,
) -> Option<crate::cpu_power::PowerLimitParams> {
    if was_sync && !reset_ok {
        previous
    } else {
        None
    }
}

impl App {
    pub(crate) fn handle_cpu_power_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::InstallPawnIO => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                if !self.begin_pawnio_task("Installing PawnIO...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::install_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOInstalled,
                ))
            }
            Message::PawnIOInstalled(result) => {
                self.pawnio_busy = false;
                if let Err(e) = result {
                    error!("PawnIO install failed: {}", e);
                    self.cpu_power_error = Some(format!("PawnIO install failed: {}", e));
                    self.mark_dirty();
                    Some(Task::none())
                } else {
                    // Reset DLL pointers so next read picks up upgraded DLL.
                    self.cpu_power_error = None;
                    self.pl_fields_dirty = false;
                    crate::cpu_power::reset_dll_fns();
                    Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {
                        crate::cpu_power::pawnio_version();
                    }))
                }
            }
            Message::DownloadPawnIOModules => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                if !self.begin_pawnio_task("Downloading PawnIO Modules...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || {
                                crate::cpu_power::download_and_extract_modules()
                                    .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOModulesDownloaded,
                ))
            }
            Message::PawnIOModulesDownloaded(result) => {
                self.pawnio_busy = false;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;
                        self.pl_fields_dirty = false;
                        Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
                    }
                    Err(e) => {
                        error!("PawnIO Modules download failed: {}", e);
                        self.modules_download_error = Some(e.to_string());
                        self.mark_dirty();
                        Some(Task::none())
                    }
                }
            }
            Message::OpenModulesDir => {
                if let Err(e) = crate::cpu_power::open_modules_dir() {
                    self.modules_download_error = Some(e);
                    self.mark_dirty();
                }
                Some(Task::none())
            }
            Message::RedetectModules => {
                let ok = crate::cpu_power::redetect_modules();
                if ok {
                    self.modules_download_error = None;
                    self.pl_fields_dirty = false;
                    Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
                } else {
                    self.modules_download_error = Some("Modules not found or hash mismatch".into());
                    self.mark_dirty();
                    Some(Task::none())
                }
            }
            Message::UpdatePawnIO => {
                tracing::info!("UpdatePawnIO triggered");
                if !self.begin_pawnio_task("Updating PawnIO...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::update_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOInstalled,
                ))
            }
            Message::UpdatePawnIOModules => {
                tracing::info!("UpdatePawnIOModules triggered");
                if !self.begin_pawnio_task("Updating PawnIO Modules...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::update_pawnio_modules().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOModulesDownloaded,
                ))
            }
            Message::UpdatePawnIOAll => {
                tracing::info!("UpdatePawnIOAll triggered");
                if !self
                    .begin_pawnio_task("Updating PawnIO & Modules... Please wait up to 2 minutes")
                {
                    return Some(Task::none());
                }
                self.cpu_power_error = None;
                Some(Task::perform(
                    async {
                        let r1 = crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::update_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err);
                        let r2 = crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::update_pawnio_modules().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err);
                        match (r1, r2) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(e1), Err(e2)) => Err(format!("PawnIO: {}; Modules: {}", e1, e2)),
                            (Err(e), _) => Err(format!("PawnIO: {}", e)),
                            (_, Err(e)) => Err(format!("Modules: {}", e)),
                        }
                    },
                    Message::UpdatePawnIOAllDone,
                ))
            }
            Message::UpdatePawnIOAllDone(result) => {
                self.pawnio_busy = false;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;
                        self.cpu_power_error = None;
                        self.pl_fields_dirty = false;
                        crate::cpu_power::reset_dll_fns();
                        return Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}));
                    }
                    Err(e) => {
                        error!("Update PawnIO & Modules failed: {}", e);
                        self.modules_download_error = Some(e.clone());
                        self.cpu_power_error = Some(e.clone());
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1Changed(val) => {
                self.pl1_edit = val.clone();
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2Changed(val) => {
                self.pl2_edit = val.clone();
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1TimeChanged(val) => {
                self.pl1_time_edit = val.clone();
                self.pl_fields_dirty = true;
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1EnabledToggled(v) => {
                self.pl1_enabled = *v;
                self.pl_fields_dirty = true;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2EnabledToggled(v) => {
                self.pl2_enabled = *v;
                self.pl_fields_dirty = true;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1ClampedToggled(v) => {
                self.pl1_clamped = *v;
                self.pl_fields_dirty = true;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2ClampedToggled(v) => {
                self.pl2_clamped = *v;
                self.pl_fields_dirty = true;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerApply => {
                // Belt and suspenders: the controls only render when a live
                // snapshot exists, but the handler re-checks so a stale or
                // raced message can never reach the write path.
                if !self.cpu_power_supported() || !self.state.cpu_power.snapshot().available {
                    return Some(Task::none());
                }
                let (pl1, pl2, pl1_time) = match self.validate_cpu_power_inputs() {
                    Ok(v) => v,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                self.pl_fields_dirty = false;
                self.cpu_power_error = None;
                let info = self.state.cpu_power.snapshot();
                let params = crate::cpu_power::PowerLimitParams {
                    pl1_watts: pl1,
                    pl1_enabled: self.pl1_enabled,
                    pl1_clamped: self.pl1_clamped,
                    pl1_time_s: pl1_time,
                    pl2_watts: pl2,
                    pl2_enabled: self.pl2_enabled,
                    pl2_clamped: self.pl2_clamped,
                    pl2_time_s: info.pl2_time_s,
                    max_time_s: info.max_time_s,
                    power_unit: info.power_unit,
                    time_unit: info.time_unit,
                };
                let was_sync = self.state.cpu_power.sync_enabled.load(Ordering::Acquire);
                let previous_sync = self.state.cpu_power.desired_sync_params();
                let cpu_power = self.state.cpu_power.clone();
                Some(Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || {
                                cpu_power.stop_sync();
                                let result = crate::cpu_power::write_msr_pl1_pl2_public(params)
                                    .map_err(|e| e.to_string());
                                if let Some(params) = sync_restart_params(
                                    was_sync,
                                    result.is_ok(),
                                    params,
                                    previous_sync,
                                ) && let Err(e) = cpu_power.start_sync(params)
                                {
                                    warn!("Failed to restart CPU power sync after apply: {}", e);
                                }
                                result
                            },
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::CpuPowerApplied,
                ))
            }
            Message::CpuPowerApplied(result) => {
                match result {
                    Ok(()) => {
                        self.pl_custom_applied.store(true, Ordering::Release);
                        self.pl_fields_dirty = false;
                        self.cpu_power_error = None;
                        return Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}));
                    }
                    Err(e) => {
                        error!("Failed to write PL1/PL2: {}", e);
                        self.cpu_power_error = Some(e.to_string());
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStart => {
                if !self.cpu_power_supported() || !self.state.cpu_power.snapshot().available {
                    return Some(Task::none());
                }
                let (pl1, pl2, pl1_time) = match self.validate_cpu_power_inputs() {
                    Ok(v) => v,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                self.pl_fields_dirty = false;
                self.cpu_power_error = None;
                let info = self.state.cpu_power.snapshot();
                let params = crate::cpu_power::PowerLimitParams {
                    pl1_watts: pl1,
                    pl1_enabled: self.pl1_enabled,
                    pl1_clamped: self.pl1_clamped,
                    pl1_time_s: pl1_time,
                    pl2_watts: pl2,
                    pl2_enabled: self.pl2_enabled,
                    pl2_clamped: self.pl2_clamped,
                    pl2_time_s: info.pl2_time_s,
                    max_time_s: info.max_time_s,
                    power_unit: info.power_unit,
                    time_unit: info.time_unit,
                };
                let cpu_power = self.state.cpu_power.clone();
                Some(Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || cpu_power.start_sync(params).map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::CpuPowerSyncStarted,
                ))
            }
            Message::CpuPowerSyncStarted(result) => {
                match result {
                    Ok(()) => {
                        self.state
                            .cpu_power
                            .sync_enabled
                            .store(true, Ordering::Release);
                        // Ensure AC->battery reset triggers for sync users.
                        self.pl_custom_applied.store(true, Ordering::Release);
                        self.cpu_power_error = None;
                        tracing::info!("CPU power sync started");
                    }
                    Err(e) => {
                        error!("Failed to start CPU power sync: {}", e);
                        self.cpu_power_error = Some(format!("Sync start failed: {}", e));
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStop => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                Some(stop_sync_task(self.state.cpu_power.clone()))
            }
            Message::CpuPowerSyncReset => {
                self.pl_fields_dirty = false;
                Some(self.handle_cpu_power_sync_reset())
            }
            Message::CpuPowerResetDone(result) => {
                match result {
                    Ok(crate::cpu_power::BiosRestore::Full) => {
                        self.cpu_power_error = None;
                        self.pl_custom_applied.store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_pending
                            .store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_retry_at_ms
                            .store(0, Ordering::Release);
                    }
                    Ok(crate::cpu_power::BiosRestore::Partial(e)) => {
                        warn!("CPU power reset partial: {}", e);
                        self.cpu_power_error =
                            Some(format!("MSR reset done; MMIO mirror may differ: {}", e));
                        self.pl_custom_applied.store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_pending
                            .store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_retry_at_ms
                            .store(0, Ordering::Release);
                    }
                    Err(e) => {
                        warn!("CPU power reset failed: {}", e);
                        self.cpu_power_error = Some(format!("Reset failed: {}", e));
                        self.state.lifecycle.pl_reset_retry_at_ms.store(
                            crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS),
                            Ordering::Release,
                        );
                    }
                }
                self.mark_dirty();
                Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
            }
            Message::CpuPowerDataRefreshed => {
                let info = self.state.cpu_power.snapshot();
                self.apply_edit_fields_from_snapshot(&info);
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStopped => {
                self.mark_dirty();
                Some(Task::none())
            }
            Message::RefreshCpuPower => {
                Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
            }
            _ => None,
        }
    }
}

impl App {
    pub(crate) fn cpu_power_supported(&self) -> bool {
        self.state.system.intel_cpu.load(Ordering::Acquire)
    }

    /// Marks a PawnIO install/download/update as in flight. Returns false when
    /// one is already running, so repeated clicks cannot spawn overlapping
    /// blocking tasks (and UAC prompts).
    fn begin_pawnio_task(&mut self, status: &str) -> bool {
        if self.pawnio_busy {
            return false;
        }
        self.pawnio_busy = true;
        self.modules_download_error = Some(status.to_string());
        self.mark_dirty();
        true
    }

    pub(crate) fn handle_cpu_power_sync_reset(&mut self) -> Task<Message> {
        if !self.cpu_power_supported() {
            return Task::none();
        }
        self.cpu_power_error = None;
        let bios = match self.state.cpu_power.bios_defaults() {
            Some(b) => b,
            None => {
                self.cpu_power_error = Some(
                    "Cannot reset: BIOS defaults not available (PawnIO may not be running)".into(),
                );
                self.mark_dirty();
                return Task::none();
            }
        };
        let was_sync = self.state.cpu_power.sync_enabled.load(Ordering::Acquire);
        let previous_sync = self.state.cpu_power.desired_sync_params();
        let cpu_power = self.state.cpu_power.clone();
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let write_result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_IO_TIMEOUT,
                    move || {
                        cpu_power.stop_sync();
                        let result =
                            crate::cpu_power::write_bios_defaults(&bios).map_err(|e| e.to_string());
                        if let Some(params) =
                            sync_restart_after_reset(was_sync, result.is_ok(), previous_sync)
                            && let Err(e) = cpu_power.start_sync(params)
                        {
                            warn!("Failed to restart CPU power sync after reset: {}", e);
                        }
                        result
                    },
                )
                .await;
                let result = match write_result {
                    Ok(inner) => inner,
                    Err(e) => Err(e),
                };
                Message::CpuPowerResetDone(result)
            },
            |msg| msg,
        )
    }

    /// Validates CPU power inputs; returns Ok((pl1, pl2, pl1_time)) or Err.
    pub(crate) fn validate_cpu_power_inputs(&self) -> Result<(f64, f64, f64), String> {
        let pl1: f64 = self
            .pl1_edit
            .parse()
            .map_err(|_| "PL1 is not a valid number".to_string())?;
        let pl2: f64 = self
            .pl2_edit
            .parse()
            .map_err(|_| "PL2 is not a valid number".to_string())?;
        let pl1_time: f64 = self
            .pl1_time_edit
            .parse()
            .map_err(|_| "PL1 time is not a valid number".to_string())?;
        if !pl1.is_finite() {
            return Err("PL1 must be a finite number".to_string());
        }
        if !pl2.is_finite() {
            return Err("PL2 must be a finite number".to_string());
        }
        if !pl1_time.is_finite() {
            return Err("PL1 time must be a finite number".to_string());
        }
        // Enabled floor mirrors the write boundary: a sub-5W *enabled*
        // limit would hard-throttle into an apparent hang. Disabled
        // limits skip the floor (including 0W), matching
        // `validate_limit_params`.
        if self.pl1_enabled && pl1 < crate::cpu_power::LIMIT_MIN_ENABLED_WATTS {
            return Err(format!(
                "PL1 must be at least {:.0}W when enabled",
                crate::cpu_power::LIMIT_MIN_ENABLED_WATTS
            ));
        }
        if self.pl2_enabled && pl2 < crate::cpu_power::LIMIT_MIN_ENABLED_WATTS {
            return Err(format!(
                "PL2 must be at least {:.0}W when enabled",
                crate::cpu_power::LIMIT_MIN_ENABLED_WATTS
            ));
        }
        // Upper bounds: per-model ceiling first (e.g. 40W on Laptop 12),
        // then the absolute 120W Desktop ceiling. Shared with the write
        // boundary so UI and encoder never disagree.
        let model_max = crate::cpu_power::platform_max_watts();
        if pl1 > model_max {
            return Err(format!(
                "PL1 must not exceed {:.0}W on this model",
                model_max
            ));
        }
        if pl2 > model_max {
            return Err(format!(
                "PL2 must not exceed {:.0}W on this model",
                model_max
            ));
        }
        if pl1 > crate::cpu_power::LIMIT_MAX_WATTS {
            return Err(format!(
                "PL1 must not exceed {:.0}W",
                crate::cpu_power::LIMIT_MAX_WATTS
            ));
        }
        if pl2 > crate::cpu_power::LIMIT_MAX_WATTS {
            return Err(format!(
                "PL2 must not exceed {:.0}W",
                crate::cpu_power::LIMIT_MAX_WATTS
            ));
        }
        if pl1_time <= 0.0 {
            return Err("PL1 time must be greater than 0s".to_string());
        }
        if pl1_time > crate::cpu_power::LIMIT_MAX_TIME_S {
            return Err(format!(
                "PL1 time must not exceed {:.0}s",
                crate::cpu_power::LIMIT_MAX_TIME_S
            ));
        }
        if self.pl1_enabled && self.pl2_enabled && pl1 > pl2 {
            return Err(format!(
                "PL1 ({:.1}W) must not exceed PL2 ({:.1}W)",
                pl1, pl2
            ));
        }
        Ok((pl1, pl2, pl1_time))
    }

    /// Populates edit fields from CPU power snapshot.
    fn apply_edit_fields_from_snapshot(&mut self, info: &crate::cpu_power::CpuPowerInfo) {
        // Do not clobber uncommitted user edits with a fresh readback (#7).
        if self.pl_fields_dirty {
            return;
        }
        let (pl1, pl2, p1en, p2en, p1cl, p2cl, t1, _t2) = info.init_edit_fields();
        self.pl1_edit = pl1;
        self.pl2_edit = pl2;
        self.pl1_time_edit = t1;
        self.pl1_enabled = p1en;
        self.pl2_enabled = p2en;
        self.pl1_clamped = p1cl;
        self.pl2_clamped = p2cl;
    }
}

#[cfg(test)]
mod tests {
    use super::sync_restart_params;
    use crate::cpu_power::PowerLimitParams;

    fn params(pl1: f64) -> PowerLimitParams {
        PowerLimitParams {
            pl1_watts: pl1,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            max_time_s: None,
            power_unit: 0.125,
            time_unit: 0.0009765625,
        }
    }

    #[test]
    fn apply_keeps_sync_disabled_when_it_was_disabled() {
        assert_eq!(
            sync_restart_params(false, true, params(15.0), Some(params(28.0))),
            None
        );
    }

    #[test]
    fn successful_apply_restarts_sync_with_new_params() {
        assert_eq!(
            sync_restart_params(true, true, params(15.0), Some(params(28.0))),
            Some(params(15.0))
        );
    }

    #[test]
    fn failed_apply_restores_previous_sync_params() {
        assert_eq!(
            sync_restart_params(true, false, params(15.0), Some(params(28.0))),
            Some(params(28.0))
        );
        assert_eq!(sync_restart_params(true, false, params(15.0), None), None);
    }
}

#[cfg(test)]
mod reset_tests {
    use super::sync_restart_after_reset;
    use crate::cpu_power::PowerLimitParams;

    fn params(pl1: f64) -> PowerLimitParams {
        PowerLimitParams {
            pl1_watts: pl1,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            max_time_s: None,
            power_unit: 0.125,
            time_unit: 0.0009765625,
        }
    }

    #[test]
    fn successful_reset_leaves_sync_stopped() {
        assert_eq!(
            sync_restart_after_reset(true, true, Some(params(28.0))),
            None
        );
    }

    #[test]
    fn failed_reset_restores_previous_sync() {
        assert_eq!(
            sync_restart_after_reset(true, false, Some(params(28.0))),
            Some(params(28.0))
        );
        assert_eq!(sync_restart_after_reset(true, false, None), None);
    }

    #[test]
    fn reset_does_not_start_sync_when_it_was_disabled() {
        assert_eq!(
            sync_restart_after_reset(false, false, Some(params(28.0))),
            None
        );
    }
}
