//! Owned macro output; Windows held shortcuts share its physical input ledger.

use openlogi_core::macros::{MacroEvent, MacroInput};
use std::collections::BTreeMap;

#[cfg(target_os = "windows")]
mod windows;

/// Native input error, surfaced to the editor.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MacroOutputError(pub String);

/// Owns the physical-output leases of one macro scheduler.
#[derive(Default)]
pub struct MacroOutput {
    leases: BTreeMap<u64, u64>,
}

impl MacroOutput {
    /// Whether native replay is implemented on this host.
    #[must_use]
    pub const fn supported() -> bool {
        cfg!(target_os = "windows")
    }

    /// Apply an input edge or wheel event; delays belong to the scheduler.
    pub fn post(&mut self, run: u64, event: &MacroEvent) -> Result<(), MacroOutputError> {
        #[cfg(target_os = "windows")]
        {
            let owner = *self.leases.entry(run).or_insert_with(windows::next_owner);
            windows::post(owner, event)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (run, event);
            Err(MacroOutputError(
                "Macro replay currently requires Windows".into(),
            ))
        }
    }

    /// Release only this run's held inputs.
    pub fn cleanup(&mut self, run: u64) -> Result<(), MacroOutputError> {
        #[cfg(target_os = "windows")]
        {
            if let Some(owner) = self.leases.get(&run) {
                windows::cleanup(*owner)?;
                self.leases.remove(&run);
            }
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.leases.remove(&run);
            Ok(())
        }
    }

    /// Release every macro lease, including a batch cancelled after logical completion.
    pub fn cleanup_all(&mut self) -> Result<(), MacroOutputError> {
        let mut failure = None;
        for run in self.leases.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = self.cleanup(run) {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for MacroOutput {
    fn drop(&mut self) {
        for run in self.leases.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = self.cleanup(run) {
                tracing::warn!(%error, "macro input release failed");
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use windows::{legacy_transition, scan_for_vk};

/// Convert a USB keyboard usage for read-only G HUB macro imports.
pub fn input_for_usage(usage: u8) -> Result<MacroInput, MacroOutputError> {
    #[cfg(target_os = "windows")]
    {
        let vk = match usage {
            0xe0 => 0xa2,
            0xe1 => 0xa0,
            0xe2 => 0xa4,
            0xe3 => 0x5b,
            0xe4 => 0xa3,
            0xe5 => 0xa1,
            0xe6 => 0xa5,
            0xe7 => 0x5c,
            _ => crate::inject::hid_usage_to_windows(usage).ok_or_else(|| {
                MacroOutputError(format!("Unsupported keyboard usage {usage:#04x}"))
            })?,
        };
        scan_for_vk(vk).ok_or_else(|| {
            MacroOutputError(format!("No scan code for keyboard usage {usage:#04x}"))
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = usage;
        Err(MacroOutputError(
            "G HUB key conversion currently requires Windows".into(),
        ))
    }
}
