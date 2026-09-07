use super::{App, Message, refresh_cpu_power_task, stop_sync_task};
use iced::Task;
use std::sync::atomic::Ordering;
use tracing::{error, warn};

impl App {
    pub(crate) fn handle_cpu_power_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::InstallPawnIO => {
                if !self.cpu_power_supported() {
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
            Message::PawnIOModulesDownloaded(result) => match result {
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
            },
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
                self.modules_download_error = Some("Updating PawnIO...".to_string());
                self.mark_dirty();
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
                self.modules_download_error = Some("Updating PawnIO Modules...".to_string());
                self.mark_dirty();
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
                self.modules_download_error =
                    Some("Updating PawnIO & Modules... Please wait 30s".to_string());
                self.cpu_power_error = None;
                self.mark_dirty();
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
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;
                        self.cpu_power_error = None;
                        self.pl_fields_dirty = false;
                        crate::cpu_power::invalidate_pawnio_version();
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
                if !self.cpu_power_supported() {
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
                let pl1_en = self.pl1_enabled;
                let pl2_en = self.pl2_enabled;
                let pl1_cl = self.pl1_clamped;
                let pl2_cl = self.pl2_clamped;
                let info = self.state.cpu_power.snapshot();
                let power_unit = info.power_unit;
                let time_unit = info.time_unit;
                let pl2_time = info.pl2_time_s;
                Some(Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || {
                                crate::cpu_power::write_msr_pl1_pl2_public(
                                    pl1, pl1_en, pl1_cl, pl1_time, pl2, pl2_en, pl2_cl, pl2_time,
                                    power_unit, time_unit,
                                )
                                .map_err(|e| e.to_string())
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
                        // Refresh readback and restart sync with fresh values if enabled.
                        let cpu_power = self.state.cpu_power.clone();
                        let after = {
                            let cpu_power = cpu_power.clone();
                            move || {
                                // Re-read flag; user may have toggled sync during refresh.
                                if !cpu_power.sync_enabled.load(Ordering::Acquire) {
                                    return;
                                }
                                let info = cpu_power.snapshot();
                                let _ = cpu_power.start_sync(
                                    info.pl1_msr,
                                    info.pl1_msr_enabled,
                                    info.pl1_msr_clamped,
                                    info.pl1_time_s,
                                    info.pl2_msr,
                                    info.pl2_msr_enabled,
                                    info.pl2_msr_clamped,
                                    info.pl2_time_s,
                                    info.power_unit,
                                    info.time_unit,
                                );
                            }
                        };
                        return Some(refresh_cpu_power_task(cpu_power, after));
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
                if !self.cpu_power_supported() {
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
                let pl1_en = self.pl1_enabled;
                let pl2_en = self.pl2_enabled;
                let pl1_cl = self.pl1_clamped;
                let pl2_cl = self.pl2_clamped;
                let info = self.state.cpu_power.snapshot();
                let power_unit = info.power_unit;
                let time_unit = info.time_unit;
                let pl2_time = info.pl2_time_s;
                let cpu_power = self.state.cpu_power.clone();
                Some(Task::perform(
                    async move {
                        let _guard = crate::util::cpu_power_mutex().lock().await;
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_IO_TIMEOUT,
                            move || {
                                cpu_power
                                    .start_sync(
                                        pl1, pl1_en, pl1_cl, pl1_time, pl2, pl2_en, pl2_cl,
                                        pl2_time, power_unit, time_unit,
                                    )
                                    .map_err(|e| e.to_string())
                            },
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
                    Ok(()) => {
                        self.cpu_power_error = None;
                    }
                    Err(e) => {
                        warn!("CPU power reset failed: {}", e);
                        self.cpu_power_error = Some(format!("Reset failed: {}", e));
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

    pub(crate) fn handle_cpu_power_sync_reset(&mut self) -> Task<Message> {
        if !self.cpu_power_supported() {
            return Task::none();
        }
        self.pl_custom_applied.store(false, Ordering::Release);
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
        let cpu_power = self.state.cpu_power.clone();
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let write_result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_IO_TIMEOUT,
                    move || {
                        cpu_power.stop_sync();
                        crate::cpu_power::write_bios_defaults(&bios).map_err(|e| e.to_string())
                    },
                )
                .await;
                let result = match write_result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e),
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
        if pl1 <= 0.0 {
            return Err("PL1 must be greater than 0W".to_string());
        }
        if pl2 <= 0.0 {
            return Err("PL2 must be greater than 0W".to_string());
        }
        // Upper bounds: beyond the 15-bit RAPL field the encoder silently
        // clamps, which then surfaces as a confusing read-back mismatch.
        const PL_MAX_WATTS: f64 = 1024.0;
        if pl1 > PL_MAX_WATTS {
            return Err(format!("PL1 must not exceed {:.0}W", PL_MAX_WATTS));
        }
        if pl2 > PL_MAX_WATTS {
            return Err(format!("PL2 must not exceed {:.0}W", PL_MAX_WATTS));
        }
        if pl1_time <= 0.0 {
            return Err("PL1 time must be greater than 0s".to_string());
        }
        const PL_TIME_MAX_S: f64 = 120.0;
        if pl1_time > PL_TIME_MAX_S {
            return Err(format!("PL1 time must not exceed {:.0}s", PL_TIME_MAX_S));
        }
        if pl1 > pl2 {
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
