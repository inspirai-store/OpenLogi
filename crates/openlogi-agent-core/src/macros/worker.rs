//! Worker-owned scheduler, recorder and native output; no hook-thread effects.
use super::{
    Ack, Command, Config, Duration, Envelope, Instant, MacroDefinition, MacroEvent, MacroHandle,
    MacroOutput, Ordering, Recorder, mpsc, set_error,
};
use openlogi_core::binding::Action;
use openlogi_core::macros::scheduler::{MacroScheduler, Playback};
use std::collections::{BTreeSet, HashSet};

#[derive(Default)]
struct Worker {
    config: Config,
    scheduler: MacroScheduler,
    output: MacroOutput,
    recorder: Recorder,
    down: HashSet<u8>,
    epoch: u64,
    pending: Option<(MacroDefinition, Instant)>,
    foreground: Option<String>,
    window: Option<usize>,
}
impl Worker {
    fn stop(&mut self, handle: &MacroHandle) {
        self.scheduler.cancel_all();
        self.pending = None;
        if let Err(error) = self.output.cleanup_all() {
            set_error(handle, error.to_string());
        }
    }
    fn press(&mut self, binding: &str, id: &str, now: u64, handle: &MacroHandle) {
        if let Some(def) = self.config.macros.library.get(id)
            && let Err(error) = self.scheduler.press(binding, id, def, now)
        {
            set_error(handle, error.to_string());
        }
    }
    fn trigger(&mut self, index: u8, pressed: bool, now: u64, handle: &MacroHandle) {
        let fresh = if pressed {
            self.down.insert(index)
        } else {
            self.down.remove(&index)
        };
        if !fresh {
            return;
        }
        let Some(device) = handle.device.read().ok().and_then(|d| d.clone()) else {
            return;
        };
        let openlogi_core::device_order::DeviceStableId::Direct { identity, .. } = &device else {
            return;
        };
        let Some(key) = self.config.resolve_device_key(&device, Some(identity)) else {
            return;
        };
        let binding = format!("{}:G{}", key.as_str(), index + 4);
        if pressed {
            let buttons = self
                .config
                .effective_bindings(key.as_str(), self.foreground.as_deref());
            if let Some(value) =
                buttons.get(&openlogi_core::macros::g604::BUTTONS[usize::from(index)])
                && let Action::RunMacro(id) = value.click_action()
            {
                self.press(&binding, &id, now, handle);
            }
        } else {
            self.scheduler.release(&binding, now);
        }
    }
    fn record(&mut self, active: bool, ack: Ack, handle: &MacroHandle) {
        self.stop(handle);
        if active {
            self.recorder = Recorder::default();
        } else if let Ok(mut state) = handle.status.lock() {
            state.recorded = self.recorder.finish();
        }
        handle
            .recording
            .store(active && MacroOutput::supported(), Ordering::Release);
        let _ = ack.send(if MacroOutput::supported() {
            Ok(())
        } else {
            Err("Macro recording currently requires Windows".into())
        });
    }
    fn command(&mut self, envelope: Envelope, now: u64, handle: &MacroHandle) -> bool {
        if matches!(
            envelope.command,
            Command::Press(..) | Command::Pulse(..) | Command::Trigger(..)
        ) && (envelope.epoch != self.epoch || envelope.window != self.window)
        {
            if let Command::Trigger(index, false) = envelope.command {
                self.down.remove(&index);
            }
            return true;
        }
        if envelope.epoch != self.epoch
            && matches!(
                envelope.command,
                Command::Press(..) | Command::Trigger(..) | Command::Preview(..)
            )
        {
            if let Command::Preview(_, _, ack) = envelope.command {
                let _ = ack.send(Err("Preview cancelled before playback".into()));
            }
            return true;
        }
        match envelope.command {
            Command::Shutdown => return false,
            Command::Configure(config) => {
                self.stop(handle);
                self.config = config;
            }
            Command::Press(binding, id) => self.press(&binding, &id, now, handle),
            Command::Pulse(binding, id) => {
                if let Some(mut def) = self.config.macros.library.get(&id).cloned() {
                    if matches!(
                        def.mode,
                        openlogi_core::macros::MacroMode::WhileHeld
                            | openlogi_core::macros::MacroMode::Sequence
                    ) {
                        def.mode = openlogi_core::macros::MacroMode::Once;
                    }
                    if let Err(error) = self.scheduler.press(&binding, &id, &def, now) {
                        set_error(handle, error.to_string());
                    }
                }
            }
            Command::Release(binding) => self.scheduler.release(&binding, now),
            Command::Cancel(binding) => {
                let steps = self.scheduler.cancel(&binding);
                self.cleanup(steps, handle);
            }
            Command::Trigger(index, pressed) => self.trigger(index, pressed, now, handle),
            Command::ResetTriggers => self.down.clear(),
            Command::Record(active, ack) => self.record(active, ack, handle),
            Command::Physical(event, at) => {
                let own_mouse_control = !matches!(
                    event,
                    MacroEvent::Down(openlogi_core::macros::MacroInput::Key { .. })
                        | MacroEvent::Up(openlogi_core::macros::MacroInput::Key { .. })
                ) && self
                    .foreground
                    .as_ref()
                    .is_some_and(|app| app.ends_with("openlogi-desktop.exe"));
                if !own_mouse_control && handle.recording.load(Ordering::Acquire) {
                    if self.recorder.events.len() + self.recorder.held.len() + 3 < 100_000 {
                        self.recorder.push(event, at);
                    } else {
                        handle.recording.store(false, Ordering::Release);
                        set_error(handle, "Recording exceeded 100000 events".into());
                    }
                }
            }
            Command::Preview(def, delay, ack) => {
                let result = def.validate().map_err(|e| e.to_string()).and_then(|()| {
                    if MacroOutput::supported() {
                        Ok(())
                    } else {
                        Err("Macro playback currently requires Windows".into())
                    }
                });
                if result.is_ok() {
                    self.pending = Some((def, Instant::now() + Duration::from_millis(delay)));
                }
                let _ = ack.send(result);
            }
        }
        true
    }
    fn cleanup(&mut self, steps: Vec<Playback>, handle: &MacroHandle) {
        for step in steps {
            if let Playback::Cleanup(owner) = step
                && let Err(error) = self.output.cleanup(owner)
            {
                set_error(handle, error.to_string());
            }
        }
    }
    fn advance(&mut self, now: u64, handle: &MacroHandle) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, at)| Instant::now() >= *at)
            && let Some((def, _)) = self.pending.take()
            && let Err(error) = self.scheduler.press("preview", "preview", &def, now)
        {
            set_error(handle, error.to_string());
        }
        let mut failed = BTreeSet::new();
        for step in self.scheduler.advance(now) {
            if handle.cancel_epoch.load(Ordering::SeqCst) != self.epoch {
                self.stop(handle);
                break;
            }
            match step {
                Playback::Event(owner, event) if !failed.contains(&owner) => {
                    if let Err(error) = self.output.post(owner, &event) {
                        set_error(handle, error.to_string());
                        self.scheduler.cancel_owner(owner);
                        let _ = self.output.cleanup(owner);
                        failed.insert(owner);
                    }
                }
                Playback::Cleanup(owner) => {
                    if let Err(error) = self.output.cleanup(owner) {
                        set_error(handle, error.to_string());
                    }
                }
                Playback::Event(..) => {}
            }
        }
    }
    fn foreground(&mut self, handle: &MacroHandle) {
        let window = openlogi_hook::foreground_window_token();
        if window != self.window || self.foreground.is_none() {
            let app = openlogi_hook::frontmost_application().map(|app| app.id);
            if window != self.window || self.foreground != app {
                self.scheduler.cancel_all();
                if let Err(error) = self.output.cleanup_all() {
                    set_error(handle, error.to_string());
                }
                self.window = window;
                self.foreground = app;
            }
        }
    }
    fn publish(&self, handle: &MacroHandle) {
        if let Ok(mut state) = handle.status.lock() {
            state.running = self.scheduler.running();
            state.recording = handle.recording.load(Ordering::Acquire);
            state.recorded_events = self.recorder.events.len();
            state.application.clone_from(&self.foreground);
        }
    }
}
pub(super) fn run(rx: &mpsc::Receiver<Envelope>, handle: &MacroHandle) {
    let origin = Instant::now();
    let mut worker = Worker::default();
    loop {
        let duration = if worker.scheduler.is_idle() && worker.pending.is_none() {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(2)
        };
        let received = rx.recv_timeout(duration);
        let now = u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        let epoch = handle.cancel_epoch.load(Ordering::SeqCst);
        if worker.epoch != epoch {
            worker.stop(handle);
            worker.epoch = epoch;
        }
        worker.foreground(handle);
        match received {
            Ok(command) => {
                if !worker.command(command, now, handle) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        worker.advance(now, handle);
        worker.publish(handle);
    }
    worker.stop(handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlogi_core::binding::{Binding, ButtonId};
    use openlogi_core::device_order::{DeviceIdentity, DeviceStableId};
    use std::sync::Arc;

    #[test]
    fn bluetooth_trigger_resolves_saved_unit_bindings_and_application_override() {
        let (tx, _rx) = mpsc::sync_channel(4);
        let handle = MacroHandle {
            tx,
            status: Arc::default(),
            emergency: std::sync::Arc::new(std::sync::RwLock::new(
                openlogi_core::macros::MacroConfig::default().emergency_stop,
            )),
            recording: Arc::default(),
            reserved: Arc::default(),
            device: std::sync::Arc::new(std::sync::RwLock::new(Some(DeviceStableId::Direct {
                vendor_id: 0x046d,
                product_id: 0xb024,
                identity: DeviceIdentity::Unit([0x4f, 0x8f, 0x5d, 0x65]),
            }))),
            cancel_epoch: Arc::default(),
        };
        let mut worker = Worker::default();
        for id in ["default", "application"] {
            worker
                .config
                .macros
                .library
                .insert(id.into(), MacroDefinition::example());
        }
        worker
            .config
            .devices
            .entry("unit:4f8f5d65".into())
            .or_default()
            .bindings
            .insert(
                ButtonId::G4,
                Binding::Single(Action::RunMacro("default".into())),
            );
        worker.config.set_per_app_binding(
            "unit:4f8f5d65",
            "exe:test.exe",
            ButtonId::G4,
            Some(Action::RunMacro("application".into())),
        );
        worker.trigger(0, true, 0, &handle);
        assert_eq!(
            worker.scheduler.running().get("unit:4f8f5d65:G4"),
            Some(&"default".into())
        );
        worker.scheduler.cancel_all();
        worker.trigger(0, false, 1, &handle);
        worker.foreground = Some("c:\\test\\test.exe".into());
        worker.trigger(0, true, 2, &handle);
        assert_eq!(
            worker.scheduler.running().get("unit:4f8f5d65:G4"),
            Some(&"application".into())
        );
    }
}
