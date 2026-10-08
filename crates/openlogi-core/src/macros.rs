//! Portable macro programs and input ownership shared by the editor and agent.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod g604;
pub mod scheduler;

/// Maximum duration of one editable delay or cycle interval.
pub const MAX_DELAY_MS: u64 = 86_400_000;

/// An output whose down/up state can be shared by several macro instances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MacroInput {
    /// Windows hardware scan code, preserving extended and left/right keys.
    Key {
        /// Set-1 scan code.
        scan_code: u16,
        /// E0-prefixed key.
        extended: bool,
    },
    /// Primary mouse button.
    Left,
    /// Secondary mouse button.
    Right,
    /// Wheel button.
    Middle,
    /// Mouse back button.
    Back,
    /// Mouse forward button.
    Forward,
}

/// One editable timeline event.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MacroEvent {
    /// Acquire a held output for this run.
    Down(MacroInput),
    /// Release this run's ownership, without releasing another run's hold.
    Up(MacroInput),
    /// Vertical/horizontal wheel delta in Windows units (120 per notch).
    Wheel {
        /// Horizontal delta.
        x: i32,
        /// Vertical delta.
        y: i32,
    },
    /// Cancellable delay in milliseconds.
    Delay(u64),
}

/// Playback behavior; declaration order is part of the IPC wire format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MacroMode {
    /// Execute the cycle once.
    #[default]
    Once,
    /// Execute the cycle exactly this many times.
    Repeat(u32),
    /// Repeat while the physical trigger remains pressed.
    WhileHeld,
    /// One trigger starts, the next trigger stops.
    Toggle,
    /// Start on press, repeat the cycle while held, finish on release.
    Sequence,
}

/// A named macro with setup, repeatable body, and normal-release cleanup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacroDefinition {
    /// Name displayed in the macro library.
    pub name: String,
    /// How a physical trigger controls the run.
    #[serde(default)]
    pub mode: MacroMode,
    /// Setup performed once before the first cycle.
    #[serde(default)]
    pub start: Vec<MacroEvent>,
    /// The repeatable timeline.
    #[serde(default)]
    pub cycle: Vec<MacroEvent>,
    /// Timeline performed on normal completion/release, before held cleanup.
    #[serde(default)]
    pub finish: Vec<MacroEvent>,
    /// Pause between cycles; prevents a zero-duration infinite loop.
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
}

const fn default_interval() -> u64 {
    50
}

/// Invalid or unbounded macro input, rejected before playback/persistence.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct MacroValidationError(pub String);

impl MacroDefinition {
    /// A balanced single-shot 12345 program with 50 ms key holds and gaps.
    #[must_use]
    pub fn example() -> Self {
        let mut cycle = Vec::new();
        for scan_code in 2..=6 {
            let key = MacroInput::Key {
                scan_code,
                extended: false,
            };
            cycle.extend([
                MacroEvent::Down(key),
                MacroEvent::Delay(50),
                MacroEvent::Up(key),
            ]);
            if scan_code != 6 {
                cycle.push(MacroEvent::Delay(50));
            }
        }
        Self {
            name: "12345".into(),
            mode: MacroMode::Once,
            start: vec![],
            cycle,
            finish: vec![],
            interval_ms: 50,
        }
    }

    /// Reject empty programs, invalid keys and excessive storage/delays.
    pub fn validate(&self) -> Result<(), MacroValidationError> {
        let invalid = |message: &str| Err(MacroValidationError(message.into()));
        if self.name.trim().is_empty() || self.name.len() > 1024 {
            return invalid("Macro name must contain 1–1024 bytes");
        }
        let count = self.start.len() + self.cycle.len() + self.finish.len();
        if count == 0 || count > 100_000 {
            return invalid("Macro must contain 1–100000 events");
        }
        if matches!(self.mode, MacroMode::Repeat(0)) {
            return invalid("Repeat count must be positive");
        }
        if self.interval_ms > MAX_DELAY_MS {
            return invalid("Cycle interval exceeds one day");
        }
        let repeating = !matches!(self.mode, MacroMode::Once | MacroMode::Repeat(1));
        if repeating && self.cycle.is_empty() {
            return invalid("Repeating macro needs a cycle");
        }
        if repeating
            && self.interval_ms == 0
            && !self
                .cycle
                .iter()
                .any(|e| matches!(e, MacroEvent::Delay(ms) if *ms > 0))
        {
            return invalid("Repeating macro needs a positive delay");
        }
        for event in self.start.iter().chain(&self.cycle).chain(&self.finish) {
            match event {
                MacroEvent::Down(MacroInput::Key { scan_code, .. })
                | MacroEvent::Up(MacroInput::Key { scan_code, .. })
                    if *scan_code == 0 || *scan_code > 255 =>
                {
                    return invalid("Scan code must be between 1 and 255");
                }
                MacroEvent::Delay(ms) if *ms > MAX_DELAY_MS => {
                    return invalid("Delay exceeds one day");
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Change explicit delays in all phases without changing input edges.
    pub fn set_uniform_delay(&mut self, millis: u64) {
        for event in self
            .start
            .iter_mut()
            .chain(&mut self.cycle)
            .chain(&mut self.finish)
        {
            if let MacroEvent::Delay(ms) = event {
                *ms = millis;
            }
        }
    }
}

/// Set the total delay between a down edge and its matching up in one phase.
/// Preserve intervening inputs; adjust delays nearest the release first.
/// A hold spanning phases has no fixed duration and is rejected.
pub fn set_hold_duration(
    events: &mut Vec<MacroEvent>,
    down: usize,
    millis: u64,
) -> Result<(), MacroValidationError> {
    if millis > MAX_DELAY_MS {
        return Err(MacroValidationError("Hold duration exceeds one day".into()));
    }
    let Some(MacroEvent::Down(input)) = events.get(down) else {
        return Err(MacroValidationError("Select a down event".into()));
    };
    let up = events
        .iter()
        .enumerate()
        .skip(down + 1)
        .find_map(|(index, event)| (event == &MacroEvent::Up(*input)).then_some(index))
        .ok_or_else(|| MacroValidationError("Matching release must be in the same phase".into()))?;
    let total = events[down + 1..up]
        .iter()
        .filter_map(|event| match event {
            MacroEvent::Delay(ms) => Some(*ms),
            _ => None,
        })
        .try_fold(0u64, u64::checked_add)
        .ok_or_else(|| MacroValidationError("Hold duration overflow".into()))?;
    if total < millis {
        if let Some(MacroEvent::Delay(delay)) = events[down + 1..up]
            .iter_mut()
            .rev()
            .find(|event| matches!(event, MacroEvent::Delay(_)))
        {
            *delay += millis - total;
        } else {
            events.insert(up, MacroEvent::Delay(millis));
        }
    } else {
        let mut remaining = total - millis;
        for event in events[down + 1..up].iter_mut().rev() {
            if let MacroEvent::Delay(delay) = event {
                let subtract = (*delay).min(remaining);
                *delay -= subtract;
                remaining -= subtract;
            }
        }
    }
    Ok(())
}

/// Saved macro library and the global emergency-stop chord.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacroConfig {
    /// Stable, locale-independent IDs referenced by bindings.
    #[serde(default)]
    pub library: BTreeMap<String, MacroDefinition>,
    /// Function-row chord intercepted before ordinary bindings.
    #[serde(default = "default_emergency")]
    pub emergency_stop: crate::config::KeyTrigger,
}

fn default_emergency() -> crate::config::KeyTrigger {
    crate::config::KeyTrigger {
        keycode: crate::config::FunctionKey::F12.keycode(),
        modifiers: crate::config::KeyModifiers {
            control: true,
            option: true,
            ..Default::default()
        },
    }
}

impl Default for MacroConfig {
    fn default() -> Self {
        Self {
            library: BTreeMap::new(),
            emergency_stop: default_emergency(),
        }
    }
}

impl MacroConfig {
    /// Whether this section can be omitted from an otherwise legacy config.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl crate::config::Config {
    /// Delete a macro and disable every saved reference, including hidden gesture arms.
    pub fn delete_macro(&mut self, id: &str) {
        use crate::binding::Action;
        self.macros.library.remove(id);
        let clear = |action: &mut Action| {
            if matches!(action, Action::RunMacro(value) if value == id) {
                *action = Action::None;
            }
        };
        self.keyboard.bindings.values_mut().for_each(clear);
        for device in self.devices.values_mut() {
            for binding in device.bindings.values_mut() {
                binding.remove_macro_reference(id);
            }
            for map in device.disabled_gestures.values_mut() {
                map.values_mut().for_each(clear);
            }
            for map in device.per_app_bindings.values_mut() {
                map.values_mut().for_each(clear);
            }
            for layout in std::iter::once(&mut device.action_ring.default)
                .chain(device.action_ring.per_app.values_mut())
            {
                layout.slots.retain(
                    |_, entry| !matches!(entry.action(), Action::RunMacro(value) if value == id),
                );
            }
        }
    }
}

/// Pure ownership ledger: repeated downs by one owner are idempotent.
#[derive(Debug, Default)]
pub struct InputOwnership {
    owners: BTreeMap<u64, BTreeSet<MacroInput>>,
    counts: BTreeMap<MacroInput, usize>,
}

impl InputOwnership {
    /// Acquire an output; true means the caller must emit its physical down.
    pub fn press(&mut self, owner: u64, input: MacroInput) -> bool {
        if !self.owners.entry(owner).or_default().insert(input) {
            return false;
        }
        let count = self.counts.entry(input).or_default();
        *count += 1;
        *count == 1
    }

    /// Release only this owner's hold; true means emit a physical up.
    pub fn release(&mut self, owner: u64, input: MacroInput) -> bool {
        if !self
            .owners
            .get_mut(&owner)
            .is_some_and(|held| held.remove(&input))
        {
            return false;
        }
        let Some(count) = self.counts.get_mut(&input) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.counts.remove(&input);
            true
        } else {
            false
        }
    }

    /// Drop an owner and return only outputs whose final owner was removed.
    pub fn release_all(&mut self, owner: u64) -> Vec<MacroInput> {
        let inputs = self.owners.get(&owner).cloned().unwrap_or_default();
        let released = inputs
            .into_iter()
            .filter(|input| self.release(owner, *input))
            .collect();
        self.owners.remove(&owner);
        released
    }
}

/// Cached macro playback state returned to IPC clients.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroStatus {
    /// Running binding IDs and their macro IDs.
    pub running: BTreeMap<String, String>,
    /// Number of physical events captured in the current recording.
    pub recorded_events: usize,
    /// Whether the recorder is armed.
    pub recording: bool,
    /// Last playback or capture error.
    pub error: Option<String>,
    /// Foreground application used to resolve the active profile.
    pub application: Option<String>,
    /// Most recently completed recording, retained until the next recording.
    pub recorded: Vec<MacroEvent>,
}

/// Append-only agent control protocol for the macro editor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MacroCommand {
    /// Fetch the persisted library without changing it.
    Library,
    /// Validate and save one definition.
    Save {
        /// Stable library identifier.
        id: String,
        /// Edited program.
        definition: MacroDefinition,
    },
    /// Remove a definition and references to it.
    Delete(String),
    /// Assign a side button, optionally in an application profile.
    Bind {
        /// Existing device configuration namespace.
        device: String,
        /// Physical side button.
        button: crate::binding::ButtonId,
        /// Application profile key, or global bindings.
        application: Option<String>,
        /// Macro identifier, or clear the binding.
        id: Option<String>,
    },
    /// Change the emergency stop chord.
    Emergency(crate::config::KeyTrigger),
    /// Arm physical input recording.
    Record,
    /// Finish recording and balance any held inputs.
    StopRecording,
    /// Preview in the foreground after a focus-changing countdown.
    Play {
        /// Program to preview.
        definition: MacroDefinition,
        /// Countdown for focusing the target field.
        delay_ms: u64,
    },
    /// Normal release of a preview or binding.
    Release(String),
    /// Immediately cancel all playback.
    StopAll,
    /// Read G HUB's database without modifying it.
    ImportGhub,
}

/// Read-only import candidate, including actions that cannot be translated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacroImport {
    /// Translated program; importing is a separate Save command.
    pub definition: MacroDefinition,
    /// Source actions omitted from the translation.
    pub unsupported: Vec<String>,
}

/// Result returned after an editor command.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MacroReply {
    /// Current persisted library and emergency chord.
    pub config: MacroConfig,
    /// Read-only G HUB import previews.
    pub imports: Vec<MacroImport>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_holds_survive_one_owner_stopping() {
        let key = MacroInput::Right;
        let mut ledger = InputOwnership::default();
        assert!(ledger.press(1, key));
        assert!(!ledger.press(1, key));
        assert!(!ledger.press(2, key));
        assert!(ledger.release_all(1).is_empty());
        assert_eq!(ledger.release_all(2), [key]);
        assert!(ledger.release_all(2).is_empty());
    }
    #[test]
    fn unowned_up_never_releases_another_run() {
        let mut ledger = InputOwnership::default();
        assert!(ledger.press(1, MacroInput::Left));
        assert!(!ledger.release(2, MacroInput::Left));
        assert_eq!(ledger.release_all(1), [MacroInput::Left]);
    }
    #[test]
    fn example_is_balanced_and_repeat_cannot_busy_loop() {
        let mut program = MacroDefinition::example();
        program.validate().unwrap();
        program.mode = MacroMode::Toggle;
        program.set_uniform_delay(0);
        program.interval_ms = 0;
        assert!(program.validate().is_err());
        program.interval_ms = 1;
        program.validate().unwrap();
    }
    #[test]
    fn hold_duration_edits_total_across_interleaved_inputs() {
        let mut events = vec![
            MacroEvent::Down(MacroInput::Right),
            MacroEvent::Delay(10),
            MacroEvent::Down(MacroInput::Left),
            MacroEvent::Delay(50),
            MacroEvent::Up(MacroInput::Left),
            MacroEvent::Delay(20),
            MacroEvent::Up(MacroInput::Right),
        ];
        set_hold_duration(&mut events, 0, 60).unwrap();
        assert_eq!(events[1], MacroEvent::Delay(10));
        assert_eq!(events[3], MacroEvent::Delay(50));
        assert_eq!(events[5], MacroEvent::Delay(0));
        set_hold_duration(&mut events, 0, 100).unwrap();
        assert_eq!(events[5], MacroEvent::Delay(40));
        set_hold_duration(&mut events, 0, 5).unwrap();
        assert_eq!(events[1], MacroEvent::Delay(5));
        assert_eq!(events[3], MacroEvent::Delay(0));
        assert_eq!(events[5], MacroEvent::Delay(0));
        assert_eq!(events[4], MacroEvent::Up(MacroInput::Left));
    }
    #[test]
    fn hold_duration_inserts_delay_and_rejects_cross_phase_or_invalid_selection() {
        let mut events = vec![
            MacroEvent::Down(MacroInput::Right),
            MacroEvent::Up(MacroInput::Right),
        ];
        set_hold_duration(&mut events, 0, 50).unwrap();
        assert_eq!(events[1], MacroEvent::Delay(50));
        let original = events.clone();
        assert!(set_hold_duration(&mut events, 1, 20).is_err());
        assert!(set_hold_duration(&mut events, 0, MAX_DELAY_MS + 1).is_err());
        assert_eq!(events, original);
        let mut held = vec![MacroEvent::Down(MacroInput::Right)];
        assert!(set_hold_duration(&mut held, 0, 50).is_err());
    }
}
