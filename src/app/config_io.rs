//! Settings export/import and fan-calibration wizard dispatch.
//!
//! Split from `misc.rs` (P8-E): the settings file dialogs plus the
//! calibration wizard lived in the catch-all misc handler. This module owns
//! `Export/Import/Calibration*` messages; everything else stays in misc.

use super::{App, Message};
use crate::util::read_lock;
use iced::Task;
use tracing::warn;

impl App {
    /// Handles settings export/import and calibration wizard messages.
    pub(crate) fn handle_config_io_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::ExportConfig => {
                // One dialog at a time; the modal Win32 dialog blocks its
                // thread, so it must never run on the iced/tokio worker.
                if self.config_io_busy {
                    return Some(Task::none());
                }
                self.config_io_busy = true;
                self.config_io_status = Some("Exporting settings…".to_string());
                self.config_io_error = None;
                let cfg = read_lock(&self.state.lifecycle.config).clone();
                self.mark_dirty();
                Some(Task::perform(
                    async move {
                        let res = crate::util::spawn_blocking_with_timeout(
                            std::time::Duration::from_secs(300),
                            move || {
                                let dest = match crate::file_dialog::pick_export_dest() {
                                    Ok(Some(p)) => p,
                                    Ok(None) => return Ok(None),
                                    Err(e) => return Err(e),
                                };
                                crate::config::export_to_path(&cfg, &dest)
                                    .map(|()| Some(dest.display().to_string()))
                            },
                        )
                        .await
                        .unwrap_or_else(Err);
                        Message::ExportConfigDone(res)
                    },
                    |msg| msg,
                ))
            }
            Message::ExportConfigDone(result) => {
                self.config_io_busy = false;
                match result {
                    Ok(Some(path)) => {
                        self.config_io_status = Some(format!("Exported to {path}"));
                    }
                    // Cancelled: drop the in-flight line, nothing to report.
                    Ok(None) => self.config_io_status = None,
                    Err(e) => {
                        warn!("Settings export failed: {}", e);
                        self.config_io_error = Some(e.clone());
                        self.config_io_status = None;
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::ImportConfig => {
                if self.config_io_busy {
                    return Some(Task::none());
                }
                self.config_io_busy = true;
                self.config_io_status = Some("Importing settings…".to_string());
                self.config_io_error = None;
                self.mark_dirty();
                Some(Task::perform(
                    async move {
                        let res = crate::util::spawn_blocking_with_timeout(
                            std::time::Duration::from_secs(300),
                            move || {
                                let src = match crate::file_dialog::pick_import_source() {
                                    Ok(Some(p)) => p,
                                    Ok(None) => return Ok(None),
                                    Err(e) => return Err(e),
                                };
                                crate::config::import_file(&src).map(|s| Some(Box::new(s)))
                            },
                        )
                        .await
                        .unwrap_or_else(Err);
                        Message::ImportConfigDone(res)
                    },
                    |msg| msg,
                ))
            }
            Message::ImportConfigDone(result) => {
                self.config_io_busy = false;
                match result {
                    Ok(Some(imported)) => {
                        // A live wizard owns EC writes: cancel it first so
                        // the import cannot race it (last-writer-wins on
                        // calibration_tables). The cancel queues an immediate
                        // firmware-handover restore; batch it below.
                        let wizard_restore: Option<Task<Message>> =
                            if self.calibration.is_some() {
                                self.calibration_cancel()
                            } else {
                                None
                            };
                        self.apply_imported_config(&imported.config);
                        // A same-chassis warning is informational, not an
                        // error: fold it into the success line so it is
                        // seen but never blocks the apply. Imports replace
                        // the whole file, so unlisted fields always reset.
                        self.config_io_status = Some(match imported.warning.as_ref() {
                            Some(w) => format!(
                                "Settings imported; unlisted fields reset to defaults ({w})"
                            ),
                            None => {
                                "Settings imported; unlisted fields reset to defaults".to_string()
                            }
                        });
                        self.mark_dirty();
                        if let Some(restore) = wizard_restore {
                            return Some(restore);
                        }
                    }
                    // Cancelled: drop the in-flight line, nothing to report.
                    Ok(None) => self.config_io_status = None,
                    Err(e) => {
                        warn!("Settings import failed: {}", e);
                        self.config_io_error = Some(e.clone());
                        self.config_io_status = None;
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CalibrationStart => self.calibration_start(),
            Message::CalibrationCancel => self.calibration_cancel(),
            Message::CalibrationClearTable => self.calibration_clear_table(),
            Message::CalibrationStepDone(result) => self.calibration_step_done(result),
            _ => None,
        }
    }
}
