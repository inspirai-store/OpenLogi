//! Durable G604 calibration and recovery, serialized independently of IPC lifetime.
use crate::hardware::DeviceAccess;
use crate::macros::MacroHandle;
use crate::observable::ObservableState;
use openlogi_core::device::DeviceInventory;
use openlogi_core::device_order::{DeviceStableId, PhysicalDeviceKey};
use openlogi_core::hid::DeviceRoute;
use openlogi_core::macros::g604::{G604Command, G604Status, OnboardBackup};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    backup: OnboardBackup,
    device: String,
    slots: [Option<u8>; 6],
}
#[derive(Serialize, Deserialize)]
struct Calibration {
    device: String,
    descriptor: Vec<u8>,
    active: Vec<u8>,
    slots: [u8; 6],
}
impl Calibration {
    fn matches(&self, journal: &Journal) -> bool {
        self.device == journal.device
            && self.descriptor == journal.backup.descriptor
            && self.active == journal.backup.active
            && self.slots.iter().all(|slot| *slot < 16)
            && self
                .slots
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == 6
    }
}
fn write_record(path: &std::path::Path, record: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = atomic_write_file::AtomicWriteFile::open(path).map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.commit().map_err(|e| e.to_string())
}

fn rollback_message(stage: &str, error: &str, restored: Result<(), String>) -> String {
    match restored {
        Ok(()) => {
            format!("{stage}: {error}. Original profile and DPI restored; trigger input disabled.")
        }
        Err(restore_error) => {
            format!("{stage}: {error}. Restore failed: {restore_error}; recovery journal retained.")
        }
    }
}
#[derive(Default)]
struct State {
    status: G604Status,
    journal: Option<Journal>,
    verifying: bool,
    verification_failure: Option<String>,
    #[cfg(target_os = "windows")]
    capture: Option<(
        openlogi_hook::raw_input::RawInput,
        std::sync::mpsc::Receiver<openlogi_hook::raw_input::RawKey>,
    )>,
}
/// Shared wizard service; only this agent accesses hardware and the recovery journal.
pub struct G604Service {
    access: DeviceAccess,
    observable: Arc<ObservableState>,
    macros: MacroHandle,
    state: Mutex<State>,
    transaction: tokio::sync::Mutex<()>,
    path: PathBuf,
}
fn route() -> DeviceRoute {
    DeviceRoute::Direct {
        vendor_id: 0x046d,
        product_id: 0xb024,
    }
}
fn physical_device(inventory: &[DeviceInventory]) -> Result<DeviceStableId, String> {
    let mut found = Vec::new();
    for receiver in inventory {
        if receiver.receiver.vendor_id == 0x046d && receiver.receiver.product_id == 0xb024 {
            for device in &receiver.paired {
                if device.online
                    && let Some(model) = &device.model_info
                {
                    let stable = DeviceStableId::from_parts(
                        Some(&route()),
                        device.slot,
                        model.serial_number.as_deref(),
                        model.unit_id,
                    );
                    if stable.physical_key().is_some() {
                        found.push(stable);
                    }
                }
            }
        }
    }
    if found.len() == 1 {
        Ok(found.remove(0))
    } else {
        Err("Connect exactly one awake G604 Bluetooth device with a stable identity".into())
    }
}
fn identity(inventory: &[DeviceInventory]) -> Result<String, String> {
    physical_device(inventory)?
        .physical_key()
        .map(PhysicalDeviceKey::into_string)
        .ok_or_else(|| "G604 has no stable physical identity".into())
}
impl G604Service {
    /// Load a recovery record before the first possible hardware modification.
    #[must_use]
    pub fn new(
        access: DeviceAccess,
        observable: Arc<ObservableState>,
        macros: MacroHandle,
    ) -> Self {
        let path = openlogi_core::paths::config_path().map_or_else(
            |_| PathBuf::from("g604-recovery.json"),
            |p| p.with_file_name("g604-recovery.json"),
        );
        let mut state = State::default();
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Journal>(&bytes) {
                Ok(journal)
                    if openlogi_hid::write::onboard::validate_backup(&journal.backup).is_ok() =>
                {
                    state.status.slots = journal.slots;
                    state.status.recovery_pending = true;
                    state.journal = Some(journal);
                }
                Ok(_) => {
                    state.status.recovery_pending = true;
                    state.status.error =
                        Some("Recovery journal has invalid geometry, sectors or CRC".into());
                }
                Err(error) => {
                    state.status.recovery_pending = true;
                    state.status.error = Some(format!("Recovery journal cannot be read: {error}"));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => state.status.error = Some(error.to_string()),
        }
        Self {
            access,
            observable,
            macros,
            state: Mutex::new(state),
            transaction: tokio::sync::Mutex::new(()),
            path,
        }
    }
    /// Process captured hardware edges and return the cached wizard state.
    pub fn status(&self) -> G604Status {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(target_os = "windows")]
        let mut state = state;
        #[cfg(target_os = "windows")]
        {
            let events = state
                .capture
                .as_ref()
                .map(|(_, rx)| rx.try_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for key in events {
                if !key.pressed {
                    continue;
                }
                let Some(index) = state.status.awaiting else {
                    continue;
                };
                if state.verifying {
                    if key.virtual_key == 0x7c + u16::from(index) {
                        state.status.verified[usize::from(index)] = true;
                        state.status.awaiting = None;
                    } else {
                        let error = format!(
                            "G{} produced VK {:x}, expected F{}",
                            index + 4,
                            key.virtual_key,
                            index + 13
                        );
                        state.status.error = Some(error.clone());
                        state.verification_failure = Some(error);
                        state.status.awaiting = None;
                    }
                } else if let Some(journal) = state.journal.as_ref() {
                    let slots = (0u8..16)
                        .filter(|slot| {
                            let offset = 32 + usize::from(*slot) * 4;
                            let raw = &journal.backup.active[offset..offset + 4];
                            raw[0..3] == [0x80, 2, 0]
                                && openlogi_inject::macro_output::input_for_usage(raw[3])
                                    .is_ok_and(|input| input == key.input)
                        })
                        .collect::<Vec<_>>();
                    if slots.len() == 1
                        && !state.status.slots.iter().enumerate().any(|(other, slot)| {
                            other != usize::from(index) && *slot == Some(slots[0])
                        })
                    {
                        state.status.slots[usize::from(index)] = Some(slots[0]);
                        state.status.awaiting = None;
                    } else {
                        state.status.error = Some(
                            "Physical key has no unique keyboard slot in the saved active profile"
                                .into(),
                        );
                    }
                }
            }
        }
        state.status.clone()
    }
    /// Cancel playback and interception as soon as the Bluetooth device disappears.
    pub fn inventory_changed(&self, inventory: &[DeviceInventory]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if (state.status.enabled || state.status.prepared)
            && identity(inventory).ok() != state.journal.as_ref().map(|j| j.device.clone())
        {
            self.macros.g604_device(None);
            state.status.enabled = false;
            state.status.awaiting = None;
            state.status.error=Some("G604 disconnected; playback cancelled. Reconnect it and restore the saved profile.".into());
        }
    }
    fn persist(&self, journal: &Journal) -> Result<(), String> {
        write_record(&self.path, journal)
    }
    /// Run one wizard command. The server retains this in a spawned transaction.
    pub async fn command(&self, command: G604Command) -> Result<G604Status, String> {
        if matches!(command, G604Command::Status) {
            let status = self.status();
            let failed = self
                .state
                .lock()
                .map_err(|e| e.to_string())?
                .verification_failure
                .take();
            if let Some(error) = failed {
                let _transaction = self.transaction.lock().await;
                let restored = self.restore().await;
                let message = match restored {
                    Ok(()) => format!(
                        "{error}. Original profile and DPI restored; trigger input disabled."
                    ),
                    Err(restore_error) => format!(
                        "{error}. Restore failed: {restore_error}; recovery journal retained."
                    ),
                };
                self.state.lock().map_err(|e| e.to_string())?.status.error = Some(message);
                return Ok(self.status());
            }
            return Ok(status);
        }
        if !cfg!(target_os = "windows") {
            return Err("G604 Bluetooth macros currently require Windows".into());
        }
        let _transaction = self.transaction.lock().await;
        tracing::info!(?command, "G604 wizard command started");
        let result = self.execute(command.clone()).await;
        if let Err(error) = &result {
            tracing::warn!(?command, %error, "G604 wizard command failed");
            self.macros.g604_device(None);
            if let Ok(mut state) = self.state.lock() {
                state.status.error = Some(error.clone());
                state.status.enabled = false;
            }
        } else {
            tracing::info!(?command, "G604 wizard command completed");
        }
        result.map(|()| self.status())
    }
    async fn execute(&self, command: G604Command) -> Result<(), String> {
        match command {
            G604Command::Begin => self.begin().await,
            G604Command::Calibrate(index) => self.arm(index, false),
            G604Command::Verify(index) => self.arm(index, true),
            G604Command::Enable => self.enable().await,
            G604Command::Accept => self.accept(),
            G604Command::Restore => self.restore().await,
            G604Command::Status => Ok(()),
        }
    }
    async fn begin(&self) -> Result<(), String> {
        if self.status().recovery_pending {
            return Err(
                "Restore the pending recovery record before starting another session".into(),
            );
        }
        let device = self.observable.read(|s| identity(&s.inventory))?;
        #[cfg(target_os = "windows")]
        let capture = openlogi_hook::raw_input::RawInput::start().map_err(|e| e.to_string())?;
        let backup = self
            .access
            .op(&route())
            .run_onboard(|channel| async move {
                openlogi_hid::write::onboard::backup_on(&channel).await
            })
            .await
            .map_err(|e| format!("Backup failed before changing the mouse: {e}"))?;
        let mut journal = Journal {
            backup,
            device,
            slots: [None; 6],
        };
        if let Ok(bytes) = std::fs::read(self.path.with_file_name("g604-calibration.json"))
            && let Ok(calibration) = serde_json::from_slice::<Calibration>(&bytes)
            && calibration.matches(&journal)
        {
            journal.slots = calibration.slots.map(Some);
        }
        self.persist(&journal)?;
        {
            let mut state = self.state.lock().map_err(|e| e.to_string())?;
            state.journal = Some(journal.clone());
            state.status = G604Status {
                recovery_pending: true,
                slots: journal.slots,
                ..Default::default()
            };
            #[cfg(target_os = "windows")]
            {
                state.capture = Some(capture);
            }
        }
        let backup = journal.backup;
        if let Err(error) = self
            .access
            .op(&route())
            .run_onboard(|channel| async move {
                openlogi_hid::write::onboard::calibration_on(&channel, &backup).await
            })
            .await
        {
            return Err(rollback_message(
                "Calibration setup failed",
                &error.to_string(),
                self.restore().await,
            ));
        }
        Ok(())
    }
    fn arm(&self, index: u8, verifying: bool) -> Result<(), String> {
        if index >= 6 {
            return Err("Side button index must be 0–5".into());
        }
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if state.journal.is_none() {
            return Err("Start and back up before calibrating".into());
        }
        if state.status.enabled || (!verifying && state.status.prepared) {
            return Err("Restore the active trigger profile before recalibrating".into());
        }
        if verifying && !state.status.prepared {
            return Err("Write the trigger profile before verifying it".into());
        }
        #[cfg(target_os = "windows")]
        if state.capture.is_none() {
            state.capture =
                Some(openlogi_hook::raw_input::RawInput::start().map_err(|e| e.to_string())?);
        }
        #[cfg(target_os = "windows")]
        if let Some((_, rx)) = &state.capture {
            for _ in rx.try_iter() {}
        }
        state.status.error = None;
        if verifying {
            state.status.verified[usize::from(index)] = false;
        }
        state.status.awaiting = Some(index);
        state.verifying = verifying;
        Ok(())
    }
    async fn enable(&self) -> Result<(), String> {
        let status = self.status();
        if status.prepared || status.enabled {
            return Err("Trigger profile is already prepared; verify it or restore it".into());
        }
        let slots = status
            .slots
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or("Calibrate all six physical side buttons first")?;
        let slots: [u8; 6] = slots.try_into().map_err(|_| "Invalid slot count")?;
        let mut journal = self
            .state
            .lock()
            .map_err(|e| e.to_string())?
            .journal
            .clone()
            .ok_or("No backup")?;
        if self.observable.read(|s| identity(&s.inventory))? != journal.device {
            return Err("Connected device does not match the recovery record".into());
        }
        journal.slots = status.slots;
        self.persist(&journal)?;
        self.state.lock().map_err(|e| e.to_string())?.journal = Some(journal.clone());
        self.macros.reserve_validation();
        if let Err(error) = self
            .access
            .op(&route())
            .run_onboard(|channel| async move {
                openlogi_hid::write::onboard::enable_on(&channel, &journal.backup, slots).await
            })
            .await
        {
            return Err(rollback_message(
                "Trigger profile setup failed",
                &error.to_string(),
                self.restore().await,
            ));
        }
        self.state
            .lock()
            .map_err(|e| e.to_string())?
            .status
            .prepared = true;
        Ok(())
    }
    fn accept(&self) -> Result<(), String> {
        let status = self.status();
        if !status.prepared || !status.verified.into_iter().all(|value| value) {
            return Err("Verify all six F13–F18 physical buttons first".into());
        }
        let device = self.observable.read(|s| physical_device(&s.inventory))?;
        let mut state = self.state.lock().map_err(|e| e.to_string())?;
        if device
            .physical_key()
            .as_ref()
            .map(PhysicalDeviceKey::as_str)
            != state.journal.as_ref().map(|j| j.device.as_str())
        {
            return Err("Connected device does not match the recovery record".into());
        }
        let journal = state.journal.as_ref().ok_or("No backup")?;
        let slots = status
            .slots
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or("No calibration")?;
        write_record(
            &self.path.with_file_name("g604-calibration.json"),
            &Calibration {
                device: journal.device.clone(),
                descriptor: journal.backup.descriptor.clone(),
                active: journal.backup.active.clone(),
                slots: slots.try_into().map_err(|_| "Invalid slot count")?,
            },
        )?;
        self.macros.g604_device(Some(device));
        state.status.enabled = true;
        Ok(())
    }
    async fn restore(&self) -> Result<(), String> {
        self.macros.g604_device(None);
        let journal = self
            .state
            .lock()
            .map_err(|e| e.to_string())?
            .journal
            .clone()
            .ok_or("No readable recovery journal")?;
        if self.observable.read(|s| identity(&s.inventory))? != journal.device {
            return Err("Reconnect the same physical G604 before restoring".into());
        }
        self.access
            .op(&route())
            .run_onboard(|channel| async move {
                openlogi_hid::write::onboard::restore_on(&channel, &journal.backup).await
            })
            .await
            .map_err(|e| e.to_string())?;
        std::fs::remove_file(&self.path).map_err(|e| e.to_string())?;
        *self.state.lock().map_err(|e| e.to_string())? = State::default();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_setup_reports_whether_rollback_completed_or_needs_recovery() {
        let restored =
            rollback_message("Trigger profile setup failed", "sector 2 timed out", Ok(()));
        assert!(restored.contains("sector 2 timed out"));
        assert!(restored.contains("Original profile and DPI restored"));
        assert!(!restored.contains("journal retained"));

        let pending = rollback_message(
            "Trigger profile setup failed",
            "sector 2 timed out",
            Err("mouse disconnected".into()),
        );
        assert!(pending.contains("sector 2 timed out"));
        assert!(pending.contains("Restore failed: mouse disconnected"));
        assert!(pending.contains("recovery journal retained"));
    }

    #[test]
    fn saved_calibration_requires_same_device_profile_geometry_and_six_unique_slots() {
        let journal = Journal {
            device: "direct:046d:b024:unit:4f8f5d65".into(),
            backup: OnboardBackup {
                route: route(),
                descriptor: vec![1, 3],
                mode: 2,
                profile: 0,
                dpi: 800,
                directory: vec![],
                spare: vec![],
                active: vec![0; 255],
            },
            slots: [None; 6],
        };
        let mut calibration = Calibration {
            device: journal.device.clone(),
            descriptor: journal.backup.descriptor.clone(),
            active: journal.backup.active.clone(),
            slots: [3, 4, 5, 6, 7, 8],
        };
        assert!(calibration.matches(&journal));
        calibration.slots[0] = 4;
        assert!(!calibration.matches(&journal));
        calibration.slots[0] = 16;
        assert!(!calibration.matches(&journal));
        calibration.slots[0] = 3;
        calibration.active[32] = 1;
        assert!(!calibration.matches(&journal));
        calibration.active[32] = 0;
        calibration.descriptor.push(0);
        assert!(!calibration.matches(&journal));
        calibration.descriptor.pop();
        calibration.device = "direct:046d:b024:unit:11111111".into();
        assert!(!calibration.matches(&journal));
    }
}
