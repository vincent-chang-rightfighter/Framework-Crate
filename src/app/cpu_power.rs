use super::{App, Message, PL_RESET_RETRY_MS, refresh_cpu_power_task, stop_sync_task};
use crate::types::PowerProfile;
use iced::Task;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{error, warn};

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
                self.pawnio_status = None;
                if let Err(e) = result {
                    error!("PawnIO install failed: {}", e);
                    self.cpu_power_error = Some(format!("PawnIO install failed: {}", e));
                    self.mark_dirty();
                    Some(Task::none())
                } else {
                    // Reset DLL pointers so next read picks up upgraded DLL.
                    self.cpu_power_error = None;

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
                self.pawnio_status = None;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;

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

                    Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
                } else {
                    self.modules_download_error = Some("Modules not found or hash mismatch".into());
                    self.mark_dirty();
                    Some(Task::none())
                }
            }
            Message::UpdatePawnIOAll => {
                tracing::info!("UpdatePawnIOAll triggered");
                if !self
                    .begin_pawnio_task("Updating PawnIO & Modules... Please wait up to 2 minutes")
                {
                    return Some(Task::none());
                }
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
                self.pawnio_status = None;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;
                        self.cpu_power_error = None;

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

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2Changed(val) => {
                self.pl2_edit = val.clone();

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1TimeChanged(val) => {
                self.pl1_time_edit = val.clone();

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1EnabledToggled(v) => {
                self.pl1_enabled = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2EnabledToggled(v) => {
                self.pl2_enabled = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1ClampedToggled(v) => {
                self.pl1_clamped = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2ClampedToggled(v) => {
                self.pl2_clamped = *v;

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
                let profile = match self.profile_from_inputs() {
                    Ok(profile) => profile,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                // Apply always saves the selected profile, even when it is not
                // the active source; only a matching source writes the MSR.
                self.store_profile(self.pl_profile_is_ac, profile.clone());
                self.save_config();
                self.cpu_power_error = None;
                self.cpu_power_status = None;
                if self
                    .current_power_source()
                    .is_some_and(|ac| ac != self.pl_profile_is_ac)
                {
                    self.cpu_power_status = Some(format!(
                        "Saved {} profile; it will apply on {}.",
                        source_label(self.pl_profile_is_ac),
                        source_label(self.pl_profile_is_ac)
                    ));
                    self.mark_dirty();
                    return Some(Task::none());
                }
                let params = match self.profile_params(&profile) {
                    Ok(params) => params,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                // Invalidate an older Start/Stop task: this explicit Apply is
                // the newest sync-affecting intent.
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                self.pl_profile_busy = true;
                self.pl_profile_retry_failures = 0;
                self.pl_profile_retry_at_ms =
                    crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                Some(self.apply_profile_task(self.pl_profile_is_ac, params, intent))
            }
            Message::CpuPowerProfileSourceChanged(ac) => {
                self.pl_profile_is_ac = *ac;
                self.pl_profile_user_selected = true;
                self.cpu_power_status = None;
                self.cpu_power_error = None;
                self.load_pl_fields_for_source();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerProfileApplied(ac, intent, result) => {
                if *intent != self.sync_intent.load(Ordering::Acquire) {
                    // A newer Apply/Start/Stop/Reset already owns the state.
                    return Some(Task::none());
                }
                self.pl_profile_busy = false;
                match result {
                    Ok(()) => {
                        self.pl_active_source = Some(*ac);
                        self.pl_profile_retry_at_ms = 0;
                        self.pl_profile_retry_failures = 0;
                        self.set_pl_custom_applied(true);
                        self.cpu_power_error = None;
                        self.cpu_power_status =
                            Some(format!("{} profile applied.", source_label(*ac)));
                        if self.pl_profile_is_ac == *ac {
                            self.load_pl_fields_for_source();
                        }
                        return Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}));
                    }
                    Err(e) if e == PROFILE_SKIPPED_SOURCE_CHANGED => {
                        // Let the next tick pick up the profile for the new
                        // source immediately instead of showing a red error.
                        self.pl_active_source = None;
                        self.pl_profile_retry_at_ms = 0;
                        self.pl_profile_retry_failures = 0;
                        self.cpu_power_status = Some(
                            "Power source changed; applying the matching profile.".to_string(),
                        );
                        self.cpu_power_error = None;
                    }
                    Err(e) if e == PROFILE_SKIPPED_STALE_INTENT => {
                        // Superseded by a newer action; leave its state alone.
                    }
                    Err(e) => {
                        error!("Failed to apply {} power profile: {}", source_label(*ac), e);
                        self.cpu_power_error = Some(e.to_string());
                        self.pl_profile_retry_failures =
                            self.pl_profile_retry_failures.saturating_add(1);
                        if self.pl_profile_retry_failures >= MAX_PROFILE_AUTO_RETRIES {
                            self.pl_profile_retry_at_ms = 0;
                            self.cpu_power_status = Some(
                                "Profile apply failed 3 times; press Apply to retry.".to_string(),
                            );
                        } else {
                            self.pl_profile_retry_at_ms =
                                crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                        }
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStart => {
                if !self.cpu_power_supported() || !self.state.cpu_power.snapshot().available {
                    return Some(Task::none());
                }
                let ac = self.current_power_source().unwrap_or(self.pl_profile_is_ac);
                // When the card is editing the active source, enabling the
                // toggle captures the current fields (unsaved edits included).
                // Otherwise use the saved profile for the active source.
                let profile = if self.pl_profile_is_ac == ac {
                    match self.profile_from_inputs() {
                        Ok(profile) => profile,
                        Err(e) => {
                            self.cpu_power_error = Some(e);
                            self.mark_dirty();
                            return Some(Task::none());
                        }
                    }
                } else {
                    match self.saved_profile(ac) {
                        Some(profile) => profile,
                        None => {
                            self.cpu_power_error =
                                Some(format!("No {} profile saved yet.", source_label(ac)));
                            self.mark_dirty();
                            return Some(Task::none());
                        }
                    }
                };
                let params = match self.profile_params(&profile) {
                    Ok(params) => params,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                self.store_profile(ac, profile);
                self.mutate_config(|cfg| cfg.cpu_power.keep_limits = true);
                self.save_config();
                self.pl_profile_is_ac = ac;
                self.pl_profile_user_selected = true;
                self.cpu_power_error = None;
                self.cpu_power_status = Some("Keep-alive on.".to_string());
                self.pl_profile_busy = true;
                self.pl_profile_retry_failures = 0;
                self.pl_profile_retry_at_ms =
                    crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                // Invalidate an older Stop queued before this start.
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(self.apply_profile_task(ac, params, intent))
            }
            Message::CpuPowerSyncStop => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                self.save_config();
                self.pl_active_source = None;
                self.pl_profile_busy = false;
                self.pl_profile_retry_failures = 0;
                self.cpu_power_status = Some("Keep-alive off.".to_string());
                self.mark_dirty();
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(stop_sync_task(
                    self.state.cpu_power.clone(),
                    Arc::clone(&self.sync_intent),
                    intent,
                ))
            }
            Message::CpuPowerSyncReset => {
                // A queued Start/Stop from before this Reset must not run after it.
                self.sync_intent.fetch_add(1, Ordering::AcqRel);
                self.pl_profile_busy = false;
                // A successful manual Reset also turns the persisted
                // keep-alive policy off, so the app does not immediately
                // re-apply the profile behind the user's back.
                self.pl_reset_disable_keep = true;
                Some(self.handle_cpu_power_sync_reset())
            }
            Message::CpuPowerResetDone(result) => {
                match result {
                    Ok(()) => {
                        self.cpu_power_error = None;
                        self.set_pl_custom_applied(false);
                        self.state
                            .lifecycle
                            .pl_reset_pending
                            .store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_retry_at_ms
                            .store(0, Ordering::Release);
                        if self.pl_reset_disable_keep {
                            self.pl_reset_disable_keep = false;
                            self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                            self.save_config();
                            self.pl_active_source = None;
                            self.cpu_power_status =
                                Some("Reset complete; keep-alive off.".to_string());
                        } else {
                            self.cpu_power_status = Some("Reset to BIOS defaults.".to_string());
                        }
                    }
                    Err(e) => {
                        self.pl_reset_disable_keep = false;
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
                self.pl_capture_in_flight = false;
                if info.available {
                    if !self.pl_profile_user_selected {
                        // Follow the live source until the user picks a profile.
                        self.pl_profile_is_ac =
                            self.current_power_source().unwrap_or(self.pl_profile_is_ac);
                    }
                    if let Some(ac) = self.current_power_source() {
                        self.pl_capture_source = Some(ac);
                        // Record the firmware wall for this source, updating
                        // only when the MMIO mirror actually changed.
                        self.capture_initial_profile(ac, &info);
                    }
                }
                self.load_pl_fields_for_source();
                if info.available {
                    // Latched once live hardware has populated the boxes;
                    // the tick loop uses it to fire exactly one catch-up
                    // refresh when hardware appears without a message.
                    self.pl_fields_primed = true;
                } else {
                    // Hardware dropped: reopen the latch so the next
                    // appearance fires a catch-up refresh again.
                    self.pl_fields_primed = false;
                }
                self.infer_pl_custom_applied();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStopped => {
                self.mark_dirty();
                Some(Task::none())
            }
            Message::RefreshCpuPower => {
                // Retry must bypass the 60s Unavailable TTL, otherwise a
                // transient PowerShell failure makes the button a no-op.
                crate::cpu_power::invalidate_authenticode();
                Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
            }
            _ => None,
        }
    }
}

fn source_label(ac: bool) -> &'static str {
    if ac { "AC" } else { "Battery" }
}

/// Sentinel returned when the power source changes while a profile write is
/// waiting on the hardware mutex; the write must not land on the new source.
const PROFILE_SKIPPED_SOURCE_CHANGED: &str = "power source changed before the profile write";
/// Sentinel for an apply superseded by a newer start/stop/reset.
const PROFILE_SKIPPED_STALE_INTENT: &str = "profile apply superseded by a newer action";

/// Automatic profile retries before waiting for a manual Apply.
const MAX_PROFILE_AUTO_RETRIES: u32 = 3;

/// True when the live source differs from the one whose profile is active.
fn profile_switch_due(active: Option<bool>, current: Option<bool>, busy: bool) -> bool {
    current.is_some() && active != current && !busy
}

fn auto_retry_allowed(failures: u32, now_ms: u64, retry_at_ms: u64) -> bool {
    failures < MAX_PROFILE_AUTO_RETRIES && now_ms >= retry_at_ms
}

/// The MMIO mirror tracking the MSR value suggests it is reflecting a custom
/// write rather than the firmware's own wall.
fn mmio_matches_msr(info: &crate::cpu_power::CpuPowerInfo) -> bool {
    (info.pl1_mmio - info.pl1_msr).abs() <= 0.05 && (info.pl2_mmio - info.pl2_msr).abs() <= 0.05
}

impl App {
    pub(crate) fn cpu_power_supported(&self) -> bool {
        self.state.system.intel_cpu.load(Ordering::Acquire)
    }

    /// Whether the About keep-alive toggle is enabled (persisted intent).
    pub(crate) fn keep_limits_enabled(&self) -> bool {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .keep_limits
    }

    fn saved_profile(&self, ac: bool) -> Option<PowerProfile> {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .profile(ac)
            .cloned()
    }

    fn saved_initial(&self, ac: bool) -> Option<PowerProfile> {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .initial(ac)
            .cloned()
    }

    fn store_profile(&mut self, ac: bool, profile: PowerProfile) {
        self.mutate_config(|cfg| *cfg.cpu_power.profile_mut(ac) = Some(profile));
    }

    /// Records the MMIO wall seen for a source, updating the stored default
    /// only when the mirror actually changes (e.g. after a BIOS update).
    fn capture_initial_profile(&mut self, ac: bool, info: &crate::cpu_power::CpuPowerInfo) {
        let Some(profile) = crate::cpu_power::initial_profile_from_mmio(info) else {
            return;
        };
        // Do not record a wall the writer would reject (e.g. a zero time
        // window); it would only make the automatic apply retry forever.
        if crate::cpu_power::validate_limit_params(&crate::cpu_power::params_for_profile(
            &profile, info,
        ))
        .is_err()
        {
            return;
        }
        // If a custom MSR write is active and the mirror tracks it exactly,
        // treat the value as the user's own limit, not a firmware baseline.
        if self.pl_custom_applied.load(Ordering::Acquire) && mmio_matches_msr(info) {
            tracing::debug!(
                "Skipping {} MMIO wall capture while custom limits mirror the MSR",
                source_label(ac)
            );
            return;
        }
        let previous = self.saved_initial(ac);
        if previous.as_ref() == Some(&profile) {
            return;
        }
        match previous {
            Some(old) => tracing::info!(
                "Updated {} MMIO power wall: PL1 {:.1}W ({:.1}s) -> {:.1}W ({:.1}s), \
                 PL2 {:.1}W -> {:.1}W",
                source_label(ac),
                old.pl1_watts,
                old.pl1_time_s,
                profile.pl1_watts,
                profile.pl1_time_s,
                old.pl2_watts,
                profile.pl2_watts
            ),
            None => tracing::info!(
                "Recorded {} MMIO power wall: PL1={:.1}W ({:.1}s, en={}, clamped={}) \
                 PL2={:.1}W (en={}, clamped={})",
                source_label(ac),
                profile.pl1_watts,
                profile.pl1_time_s,
                profile.pl1_enabled,
                profile.pl1_clamped,
                profile.pl2_watts,
                profile.pl2_enabled,
                profile.pl2_clamped
            ),
        }
        self.mutate_config(|cfg| *cfg.cpu_power.initial_mut(ac) = Some(profile));
        self.save_config();
        self.mark_dirty();
        // A freshly captured wall is the default profile for this source:
        // let the next tick apply it when keep-alive is on and the user has
        // not saved a profile yet.
        if self.keep_limits_enabled()
            && self.current_power_source() == Some(ac)
            && self.saved_profile(ac).is_none()
        {
            self.pl_active_source = None;
        }
    }

    fn profile_from_inputs(&self) -> Result<PowerProfile, String> {
        let (pl1, pl2, pl1_time) = self.validate_cpu_power_inputs()?;
        Ok(PowerProfile {
            pl1_watts: pl1,
            pl1_enabled: self.pl1_enabled,
            pl1_clamped: self.pl1_clamped,
            pl1_time_s: pl1_time,
            pl2_watts: pl2,
            pl2_enabled: self.pl2_enabled,
            pl2_clamped: self.pl2_clamped,
        })
    }

    fn profile_params(
        &self,
        profile: &PowerProfile,
    ) -> Result<crate::cpu_power::PowerLimitParams, String> {
        let info = self.state.cpu_power.snapshot();
        let params = crate::cpu_power::params_for_profile(profile, &info);
        crate::cpu_power::validate_limit_params(&params).map(|()| params)
    }

    fn show_profile_fields(&mut self, profile: &PowerProfile) {
        self.pl1_edit = format!("{:.1}", profile.pl1_watts);
        self.pl2_edit = format!("{:.1}", profile.pl2_watts);
        self.pl1_time_edit = format!("{:.1}", profile.pl1_time_s);
        self.pl1_enabled = profile.pl1_enabled;
        self.pl2_enabled = profile.pl2_enabled;
        self.pl1_clamped = profile.pl1_clamped;
        self.pl2_clamped = profile.pl2_clamped;
    }

    /// Loads the selected profile into the edit boxes: the user's saved
    /// profile first, then the source's one-time MMIO capture, then the live
    /// registers. This keeps the default wall aligned with what the firmware
    /// enforces when plugged in or on battery.
    fn load_pl_fields_for_source(&mut self) {
        if let Some(profile) = self
            .saved_profile(self.pl_profile_is_ac)
            .or_else(|| self.saved_initial(self.pl_profile_is_ac))
        {
            self.show_profile_fields(&profile);
        } else {
            let info = self.state.cpu_power.snapshot();
            self.apply_edit_fields_from_snapshot(&info);
        }
    }

    pub(crate) fn current_power_source(&self) -> Option<bool> {
        crate::cpu_power::read_ac_present()
    }

    fn apply_profile_task(
        &self,
        ac: bool,
        params: crate::cpu_power::PowerLimitParams,
        intent: u64,
    ) -> Task<Message> {
        let cpu_power = self.state.cpu_power.clone();
        let config = Arc::clone(&self.state.lifecycle.config);
        let sync_intent = Arc::clone(&self.sync_intent);
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_SYNC_TIMEOUT,
                    move || {
                        if sync_intent.load(Ordering::Acquire) != intent {
                            return Err(PROFILE_SKIPPED_STALE_INTENT.to_string());
                        }
                        // The source can change while this task waits for the
                        // mutex; never write the old source's profile onto the
                        // new state. Leave sync untouched in that case.
                        if crate::cpu_power::read_ac_present().is_some_and(|current| current != ac)
                        {
                            return Err(PROFILE_SKIPPED_SOURCE_CHANGED.to_string());
                        }
                        let was_sync = cpu_power.sync_enabled.load(Ordering::Acquire);
                        let previous_sync = cpu_power.desired_sync_params();
                        cpu_power.stop_sync();
                        let result = crate::cpu_power::write_msr_pl1_pl2_public(params)
                            .map_err(|e| e.to_string());
                        if result.is_ok() {
                            // Re-read the toggle after taking the mutex: a Stop
                            // clicked while this task waited must win.
                            let keep = crate::util::read_lock(&config).cpu_power.keep_limits;
                            if (keep || was_sync)
                                && let Err(e) = cpu_power.start_sync(params)
                            {
                                warn!("Failed to restart CPU power sync after profile apply: {e}");
                            }
                        } else if was_sync
                            && let Some(previous) = previous_sync
                            && let Err(e) = cpu_power.start_sync(previous)
                        {
                            warn!("Failed to restore CPU power sync after profile apply: {e}");
                        }
                        result
                    },
                )
                .await
                .unwrap_or_else(Err);
                Message::CpuPowerProfileApplied(ac, intent, result)
            },
            |msg| msg,
        )
    }

    /// Applies the saved profile for the current power source when the
    /// keep-alive toggle is on and the source changed (or at startup). A
    /// source without a profile falls back to the existing BIOS reset path.
    pub(crate) fn maybe_auto_apply_profile(&mut self, now_ms: u64) -> Option<Task<Message>> {
        if !self.cpu_power_supported()
            || !self.state.cpu_power.snapshot().available
            || !self.keep_limits_enabled()
        {
            return None;
        }
        let current = self.current_power_source();
        if !profile_switch_due(self.pl_active_source, current, self.pl_profile_busy) {
            return None;
        }
        let ac = current?;
        if !auto_retry_allowed(
            self.pl_profile_retry_failures,
            now_ms,
            self.pl_profile_retry_at_ms,
        ) {
            return None;
        }
        // Use the user profile when present, otherwise the one-time MMIO wall
        // recorded for this source.
        let Some(profile) = self.saved_profile(ac).or_else(|| self.saved_initial(ac)) else {
            self.pl_active_source = Some(ac);
            if self.pl_custom_applied.load(Ordering::Acquire) {
                // Never leave the previous source's limits enforced after a
                // switch: fall back to the factory values like the legacy
                // AC→battery path.
                self.state
                    .lifecycle
                    .pl_reset_pending
                    .store(true, Ordering::Release);
                self.cpu_power_status = Some(format!(
                    "No {} profile saved; resetting to BIOS defaults.",
                    source_label(ac)
                ));
            } else {
                self.cpu_power_status = Some(format!("No {} profile saved yet.", source_label(ac)));
            }
            self.mark_dirty();
            return None;
        };
        let params = match self.profile_params(&profile) {
            Ok(params) => params,
            Err(e) => {
                self.cpu_power_error = Some(e);
                self.pl_profile_retry_at_ms = now_ms.saturating_add(PL_RESET_RETRY_MS);
                self.mark_dirty();
                return None;
            }
        };
        self.cpu_power_error = None;
        self.pl_profile_busy = true;
        self.pl_profile_retry_at_ms = now_ms.saturating_add(PL_RESET_RETRY_MS);
        let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
        Some(self.apply_profile_task(ac, params, intent))
    }

    /// Updates the in-memory flag and persists it so an app restart on battery
    /// still knows that a custom MSR value may be active.
    pub(crate) fn set_pl_custom_applied(&mut self, applied: bool) {
        if self.pl_custom_applied.swap(applied, Ordering::Release) == applied {
            return;
        }
        self.mutate_config(|cfg| cfg.pl_custom_applied = applied);
        self.save_config();
    }

    /// Recover the memory-only custom-limit flag by comparing the live
    /// registers with the persisted BIOS snapshot. Called at startup and on
    /// EC recovery so an app restart does not disarm the AC->battery reset.
    pub(crate) fn infer_pl_custom_applied(&mut self) {
        if !self.cpu_power_supported() {
            return;
        }
        // Only compare on AC. Firmware may apply different limits on battery,
        // and treating that as a user custom write would make the AC->battery
        // reset overwrite the firmware's battery-mode limits with AC values.
        if crate::cpu_power::read_ac_present() != Some(true) {
            return;
        }
        let info = self.state.cpu_power.snapshot();
        let custom = self
            .state
            .cpu_power
            .bios_defaults()
            .is_some_and(|bios| crate::cpu_power::pl_limits_differ_from_bios(&info, &bios));
        self.set_pl_custom_applied(custom);
    }

    /// Marks a PawnIO install/download/update as in flight. Returns false when
    /// one is already running, so repeated clicks cannot spawn overlapping
    /// blocking tasks (and UAC prompts).
    fn begin_pawnio_task(&mut self, status: &str) -> bool {
        if self.pawnio_busy {
            return false;
        }
        self.pawnio_busy = true;
        self.pawnio_status = Some(status.to_string());
        // A retry should not keep showing the previous failure next to the new
        // in-progress status.
        self.modules_download_error = None;
        self.cpu_power_error = None;
        self.mark_dirty();
        true
    }

    pub(crate) fn handle_cpu_power_sync_reset(&mut self) -> Task<Message> {
        if !self.cpu_power_supported() {
            // A manual Reset that never starts must not leave the flag armed
            // for a later automatic reset.
            self.pl_reset_disable_keep = false;
            return Task::none();
        }
        self.cpu_power_error = None;
        let bios = match self.state.cpu_power.bios_defaults() {
            Some(b) => b,
            None => {
                self.pl_reset_disable_keep = false;
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
                    crate::util::PAWNIO_SYNC_TIMEOUT,
                    move || {
                        // Re-read intent after acquiring the mutex so a Stop
                        // pressed while we waited is not undone.
                        let was_sync = cpu_power.sync_enabled.load(Ordering::Acquire);
                        let previous_sync = cpu_power.desired_sync_params();
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
    ///
    /// String parsing and finiteness are UI-specific. All range/order rules
    /// defer to the write boundary (`validate_limit_values`) so the UI
    /// verdict and the MSR encoder can never drift apart. Units stay
    /// unchecked here: they come from the live snapshot and may be zero
    /// before the first read; `validate_limit_params` re-checks everything,
    /// including units, at write time.
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
        // Same construction as the Apply/SyncStart handlers below (snapshot
        // supplies the fields the edit boxes do not cover). Before the first
        // successful read there is no snapshot yet (Apply no-ops then too),
        // so judge the inputs symmetrically instead of failing on the zeroed
        // placeholder PL2 time.
        let info = self.state.cpu_power.snapshot();
        let pl2_time = if info.available {
            info.pl2_time_s
        } else {
            pl1_time
        };
        let params = crate::cpu_power::PowerLimitParams {
            pl1_watts: pl1,
            pl1_enabled: self.pl1_enabled,
            pl1_clamped: self.pl1_clamped,
            pl1_time_s: pl1_time,
            pl2_watts: pl2,
            pl2_enabled: self.pl2_enabled,
            pl2_clamped: self.pl2_clamped,
            pl2_time_s: pl2_time,
            max_time_s: info.max_time_s,
            power_unit: info.power_unit,
            time_unit: info.time_unit,
        };
        crate::cpu_power::validate_limit_values(&params)?;
        Ok((pl1, pl2, pl1_time))
    }

    /// Populates edit fields from CPU power snapshot. Always overwrites:
    /// this only runs on explicit refresh paths (Refresh button, Apply /
    /// Reset completion, module events), never on the background poll, so
    /// in-progress typing is never clobbered while the card always shows
    /// the latest hardware values after hardware-changing operations.
    fn apply_edit_fields_from_snapshot(&mut self, info: &crate::cpu_power::CpuPowerInfo) {
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
    use crate::cpu_power::CpuPowerInfo;

    #[test]
    fn refresh_always_overwrites_edit_fields() {
        // Edit boxes follow the hardware on every explicit refresh; there is
        // no dirty flag to get stuck (background polls never touch them).
        let (mut app, _) = crate::App::new(false);
        app.pl1_edit = "stale".into();
        app.pl1_enabled = false;
        let info = CpuPowerInfo {
            available: true,
            pl1_msr: 30.0,
            pl1_msr_enabled: true,
            pl2_msr: 35.0,
            ..Default::default()
        };
        app.apply_edit_fields_from_snapshot(&info);
        assert_eq!(app.pl1_edit, "30.0");
        assert_eq!(app.pl2_edit, "35.0");
        assert!(app.pl1_enabled);
    }

    #[test]
    fn primed_latches_only_on_live_hardware() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        assert!(!app.pl_fields_primed);
        // Unavailable snapshot: boxes cleared, latch stays open so the tick
        // loop keeps firing catch-up refreshes.
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        assert!(!app.pl_fields_primed);
    }

    #[test]
    fn primed_reopens_when_hardware_drops() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        // Flap: a latched latch must reopen when hardware drops so the next
        // appearance fires another catch-up refresh (test harness has no
        // PawnIO, so the snapshot reads unavailable).
        app.pl_fields_primed = true;
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        assert!(!app.pl_fields_primed);
    }

    #[test]
    fn profile_switch_is_due_only_for_a_known_different_source() {
        use super::profile_switch_due;
        assert!(profile_switch_due(None, Some(true), false));
        assert!(profile_switch_due(Some(true), Some(false), false));
        assert!(!profile_switch_due(Some(true), Some(true), false));
        assert!(!profile_switch_due(None, None, false));
        assert!(!profile_switch_due(None, Some(true), true));
    }

    #[test]
    fn auto_retry_stops_after_three_failures_and_respects_backoff() {
        use super::auto_retry_allowed;
        assert!(auto_retry_allowed(0, 1000, 1000));
        assert!(!auto_retry_allowed(0, 999, 1000));
        assert!(auto_retry_allowed(2, 2000, 1000));
        assert!(!auto_retry_allowed(3, 2000, 1000));
    }

    #[test]
    fn mirror_match_has_a_small_tolerance() {
        use super::mmio_matches_msr;
        let mut info = CpuPowerInfo {
            pl1_msr: 30.0,
            pl1_mmio: 30.01,
            pl2_msr: 60.0,
            pl2_mmio: 59.99,
            ..Default::default()
        };
        assert!(mmio_matches_msr(&info));
        info.pl2_mmio = 60.2;
        assert!(!mmio_matches_msr(&info));
    }

    #[test]
    fn data_refresh_prefers_saved_profile_then_the_mmio_capture() {
        use crate::app::Message;
        use crate::types::PowerProfile;
        let (mut app, _) = crate::App::new(false);
        app.mutate_config(|cfg| {
            cfg.cpu_power.ac = None;
            cfg.cpu_power.ac_initial = Some(PowerProfile {
                pl1_watts: 28.0,
                pl2_watts: 55.0,
                ..Default::default()
            });
        });
        app.pl_profile_is_ac = true;
        app.pl_profile_user_selected = true;
        app.pl1_edit = "stale".into();
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        // No PawnIO in the test harness: the saved/captured default is the
        // only source for the boxes, and it must win over stale fields.
        assert_eq!(app.pl1_edit, "28.0");
        assert_eq!(app.pl2_edit, "55.0");
    }

    #[test]
    fn switching_source_loads_the_saved_profile_fields() {
        use crate::types::PowerProfile;
        let (mut app, _) = crate::App::new(false);
        app.store_profile(
            false,
            PowerProfile {
                pl1_watts: 22.0,
                pl1_enabled: false,
                pl1_clamped: true,
                pl1_time_s: 15.0,
                pl2_watts: 44.0,
                pl2_enabled: true,
                pl2_clamped: false,
            },
        );
        app.pl_profile_is_ac = false;
        app.load_pl_fields_for_source();
        assert_eq!(app.pl1_edit, "22.0");
        assert_eq!(app.pl2_edit, "44.0");
        assert_eq!(app.pl1_time_edit, "15.0");
        assert!(!app.pl1_enabled);
        assert!(app.pl1_clamped);
    }

    #[test]
    fn reset_that_cannot_start_disarms_the_manual_keep_flag() {
        let (mut app, _) = crate::App::new(false);
        app.pl_reset_disable_keep = true;
        let _ = app.handle_cpu_power_sync_reset();
        assert!(!app.pl_reset_disable_keep);
    }

    #[tokio::test]
    async fn begin_pawnio_task_uses_status_not_error() {
        let (mut app, _) = crate::App::new(false);
        assert!(app.begin_pawnio_task("Installing PawnIO..."));
        assert!(app.pawnio_busy);
        assert_eq!(app.pawnio_status.as_deref(), Some("Installing PawnIO..."));
        assert!(app.modules_download_error.is_none());
        assert!(!app.begin_pawnio_task("again"));
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
