//! Off-hook macro recording and cancellable playback on a single output worker.
use openlogi_core::config::{Config, FunctionKey, KeyTrigger};
use openlogi_core::macros::{MacroDefinition, MacroEvent, MacroInput, MacroStatus};
use openlogi_hook::{EventDisposition, KeyEvent, MouseEvent};
use openlogi_inject::macro_output::MacroOutput;
use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

type Ack = oneshot::Sender<Result<(), String>>;
mod worker;
struct Envelope {
    epoch: u64,
    window: Option<usize>,
    command: Command,
}
enum Command {
    Configure(Config),
    Press(String, String),
    Pulse(String, String),
    Release(String),
    Cancel(String),
    Physical(MacroEvent, Instant),
    Trigger(u8, bool),
    ResetTriggers,
    Record(bool, Ack),
    Preview(MacroDefinition, u64, Ack),
    Shutdown,
}

/// Cloneable nonblocking handle used by hook producers and IPC handlers.
#[derive(Clone)]
pub struct MacroHandle {
    tx: mpsc::SyncSender<Envelope>,
    status: Arc<Mutex<MacroStatus>>,
    emergency: Arc<RwLock<KeyTrigger>>,
    recording: Arc<AtomicBool>,
    reserved: Arc<AtomicBool>,
    device: Arc<RwLock<Option<openlogi_core::device_order::DeviceStableId>>>,
    cancel_epoch: Arc<AtomicU64>,
}

impl MacroHandle {
    fn send(&self, command: Command) {
        if self
            .tx
            .try_send(Envelope {
                epoch: self.cancel_epoch.load(Ordering::SeqCst),
                window: openlogi_hook::foreground_window_token(),
                command,
            })
            .is_err()
        {
            self.cancel_epoch.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Configure the library after startup or a successful persisted reload.
    pub fn configure(&self, config: Config) {
        if let Ok(mut emergency) = self.emergency.write() {
            *emergency = config.macros.emergency_stop.clone();
        }
        self.stop_all();
        self.send(Command::Configure(config));
    }
    /// Associate verified G604 triggers with their existing configuration key.
    pub fn g604_device(&self, device: Option<openlogi_core::device_order::DeviceStableId>) {
        self.stop_all();
        self.reserved.store(device.is_some(), Ordering::Release);
        if let Ok(mut target) = self.device.write() {
            *target = device;
        }
        self.send(Command::ResetTriggers);
    }
    /// Suppress dedicated keys during verification without executing bindings.
    pub fn reserve_validation(&self) {
        self.g604_device(None);
        self.reserved.store(true, Ordering::Release);
    }
    /// Cancel all runs before any further input batch is emitted.
    pub fn stop_all(&self) {
        self.cancel_epoch.fetch_add(1, Ordering::SeqCst);
    }
    /// Start a binding, ignoring repeat downs for a finite run.
    pub fn press(&self, binding: String, id: String) {
        self.send(Command::Press(binding, id));
    }
    /// Dispatch without a physical release; held modes degrade to one balanced cycle.
    pub fn pulse(&self, binding: String, id: String) {
        self.send(Command::Pulse(binding, id));
    }
    /// Finish a held/sequence binding normally.
    pub fn release(&self, binding: String) {
        self.send(Command::Release(binding));
    }
    /// Cancel a binding without running its finish phase.
    pub fn cancel(&self, binding: String) {
        self.send(Command::Cancel(binding));
    }
    /// Read cached state without touching the hook or HID channel.
    #[must_use]
    pub fn status(&self) -> MacroStatus {
        self.status
            .lock()
            .map_or_else(|_| MacroStatus::default(), |s| s.clone())
    }
    /// Arm or finish physical recording. Completion confirms the worker handled it.
    #[must_use]
    pub fn record(&self, active: bool) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Record(active, tx));
        rx
    }
    /// Queue playback after a countdown so the user can focus the test field.
    #[must_use]
    pub fn preview(
        &self,
        definition: MacroDefinition,
        delay_ms: u64,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.send(Command::Preview(definition, delay_ms.min(10_000), tx));
        rx
    }
    /// Capture one physical keyboard edge; dedicated triggers bypass the old shortcut path.
    #[must_use]
    pub fn physical_key(
        &self,
        input: MacroInput,
        pressed: bool,
        key: Option<KeyEvent>,
    ) -> EventDisposition {
        if let Some(key) = key {
            if key.pressed
                && self.emergency.try_read().is_ok_and(|chord| {
                    *chord
                        == KeyTrigger {
                            keycode: key.keycode,
                            modifiers: key.modifiers,
                        }
                })
            {
                self.stop_all();
                return EventDisposition::Suppress;
            }
            if self.reserved.load(Ordering::Acquire) {
                for (index, function) in [
                    FunctionKey::F13,
                    FunctionKey::F14,
                    FunctionKey::F15,
                    FunctionKey::F16,
                    FunctionKey::F17,
                    FunctionKey::F18,
                ]
                .iter()
                .enumerate()
                {
                    if key.keycode == function.keycode() {
                        self.send(Command::Trigger(u8::try_from(index).unwrap_or(0), pressed));
                        return EventDisposition::Suppress;
                    }
                }
            }
        }
        if self.recording.load(Ordering::Acquire) {
            self.send(Command::Physical(
                if pressed {
                    MacroEvent::Down(input)
                } else {
                    MacroEvent::Up(input)
                },
                Instant::now(),
            ));
        }
        EventDisposition::PassThrough
    }
    /// Record mouse clicks and wheel deltas; movement is intentionally omitted.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "wheel values are bounded before conversion to native units"
    )]
    pub fn observe_mouse(&self, event: &MouseEvent) {
        if matches!(event, MouseEvent::CaptureInterrupted) {
            self.stop_all();
        }
        if !self.recording.load(Ordering::Acquire) {
            return;
        }
        let event = match event {
            MouseEvent::Button { id, pressed, .. } => {
                use openlogi_core::binding::ButtonId as B;
                let input = match id {
                    B::LeftClick => MacroInput::Left,
                    B::RightClick => MacroInput::Right,
                    B::MiddleClick => MacroInput::Middle,
                    B::Back => MacroInput::Back,
                    B::Forward => MacroInput::Forward,
                    _ => return,
                };
                if *pressed {
                    MacroEvent::Down(input)
                } else {
                    MacroEvent::Up(input)
                }
            }
            MouseEvent::Scroll { delta, .. } => MacroEvent::Wheel {
                x: (delta.x() * 120.0).clamp(-1_000_000.0, 1_000_000.0) as i32,
                y: (delta.y() * 120.0).clamp(-1_000_000.0, 1_000_000.0) as i32,
            },
            _ => return,
        };
        self.send(Command::Physical(event, Instant::now()));
    }
}

/// Unique worker owner; shutdown cancels inputs and joins the worker.
pub struct MacroRuntime {
    handle: MacroHandle,
    worker: Option<JoinHandle<()>>,
}
impl MacroRuntime {
    /// Start an idle worker; it emits nothing until commanded.
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(1024);
        let handle = MacroHandle {
            tx,
            status: Arc::default(),
            emergency: Arc::new(RwLock::new(
                openlogi_core::macros::MacroConfig::default().emergency_stop,
            )),
            recording: Arc::default(),
            reserved: Arc::default(),
            device: Arc::default(),
            cancel_epoch: Arc::default(),
        };
        let shared = handle.clone();
        let worker = thread::Builder::new()
            .name("macro-playback".into())
            .spawn(move || worker::run(&rx, &shared))?;
        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }
    /// Return a non-owning command handle.
    #[must_use]
    pub fn handle(&self) -> MacroHandle {
        self.handle.clone()
    }
    /// Cancel and join, including any inputs held by this worker.
    pub fn shutdown(&mut self) {
        self.handle.stop_all();
        if let Some(worker) = self.worker.take() {
            let _ = self.handle.tx.send(Envelope {
                epoch: self.handle.cancel_epoch.load(Ordering::SeqCst),
                window: None,
                command: Command::Shutdown,
            });
            let _ = worker.join();
        }
    }
}
impl Drop for MacroRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<MacroEvent>,
    held: BTreeSet<MacroInput>,
    last: Option<Instant>,
}
impl Recorder {
    fn push(&mut self, event: MacroEvent, at: Instant) {
        let accepted = match event {
            MacroEvent::Down(input) => self.held.insert(input),
            MacroEvent::Up(input) => self.held.remove(&input),
            _ => true,
        };
        if !accepted {
            return;
        }
        if let Some(last) = self.last {
            self.events.push(MacroEvent::Delay(
                u64::try_from(at.saturating_duration_since(last).as_millis()).unwrap_or(0),
            ));
        }
        self.events.push(event);
        self.last = Some(at);
    }
    fn finish(&mut self) -> Vec<MacroEvent> {
        self.events.extend(
            std::mem::take(&mut self.held)
                .into_iter()
                .map(MacroEvent::Up),
        );
        std::mem::take(&mut self.events)
    }
}

fn set_error(handle: &MacroHandle, error: String) {
    if let Ok(mut state) = handle.status.lock() {
        state.error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recording_ignores_repeat_and_balances_missing_release() {
        let mut recorder = Recorder::default();
        let at = Instant::now();
        recorder.push(MacroEvent::Down(MacroInput::Right), at);
        recorder.push(MacroEvent::Down(MacroInput::Right), at);
        recorder.push(MacroEvent::Up(MacroInput::Left), at);
        assert_eq!(
            recorder.finish(),
            vec![
                MacroEvent::Down(MacroInput::Right),
                MacroEvent::Up(MacroInput::Right)
            ]
        );
    }
}
