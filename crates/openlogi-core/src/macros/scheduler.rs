//! Logical-time macro scheduler. The agent owns its clock and physical output.

use std::collections::BTreeMap;

use super::{MacroDefinition, MacroEvent, MacroMode, MacroValidationError};

/// One ordered instruction for the injector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playback {
    /// Apply an input event using this run's output ownership.
    Event(u64, MacroEvent),
    /// Release every output still owned by a terminal run.
    Cleanup(u64),
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Start,
    Cycle,
    Finish,
}

#[derive(Debug)]
struct Run {
    owner: u64,
    macro_id: String,
    program: MacroDefinition,
    phase: Phase,
    event_ix: usize,
    cycles: u32,
    deadline: u64,
}

impl Run {
    fn finish(&mut self, now: u64) {
        self.phase = Phase::Finish;
        self.event_ix = 0;
        self.deadline = now;
    }

    fn events(&self) -> &[MacroEvent] {
        match self.phase {
            Phase::Start => &self.program.start,
            Phase::Cycle => &self.program.cycle,
            Phase::Finish => &self.program.finish,
        }
    }

    fn advance(&mut self, now: u64, output: &mut Vec<Playback>) -> bool {
        // A zero-delay timeline must still yield to stop/foreground commands.
        for _ in 0..256 {
            if now < self.deadline {
                return true;
            }
            if let Some(event) = self.events().get(self.event_ix).cloned() {
                self.event_ix += 1;
                match event {
                    MacroEvent::Delay(ms) => {
                        self.deadline = now.saturating_add(ms);
                    }
                    event => output.push(Playback::Event(self.owner, event)),
                }
            } else {
                self.event_ix = 0;
                match self.phase {
                    Phase::Start => self.phase = Phase::Cycle,
                    Phase::Cycle => {
                        self.cycles = self.cycles.saturating_add(1);
                        let complete = match self.program.mode {
                            MacroMode::Once => true,
                            MacroMode::Repeat(count) => self.cycles >= count,
                            MacroMode::WhileHeld | MacroMode::Toggle | MacroMode::Sequence => false,
                        };
                        if complete {
                            self.phase = Phase::Finish;
                        } else {
                            self.deadline = now.saturating_add(self.program.interval_ms);
                        }
                    }
                    Phase::Finish => {
                        output.push(Playback::Cleanup(self.owner));
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Owns each binding's run; stopping one binding never cancels unrelated runs.
#[derive(Debug, Default)]
pub struct MacroScheduler {
    runs: BTreeMap<String, Run>,
    next_owner: u64,
}

impl MacroScheduler {
    /// Accept a distinct physical down edge or ignore a running finite macro.
    pub fn press(
        &mut self,
        binding: &str,
        id: &str,
        program: &MacroDefinition,
        now: u64,
    ) -> Result<(), MacroValidationError> {
        if let Some(run) = self.runs.get_mut(binding) {
            if run.program.mode == MacroMode::Toggle && !matches!(run.phase, Phase::Finish) {
                run.finish(now);
            }
            return Ok(());
        }
        program.validate()?;
        self.next_owner = self
            .next_owner
            .checked_add(1)
            .ok_or_else(|| MacroValidationError("Macro run IDs exhausted".into()))?;
        self.runs.insert(
            binding.into(),
            Run {
                owner: self.next_owner,
                macro_id: id.into(),
                program: program.clone(),
                phase: Phase::Start,
                event_ix: 0,
                cycles: 0,
                deadline: now,
            },
        );
        Ok(())
    }

    /// Physical release preempts delays in hold/sequence modes only.
    pub fn release(&mut self, binding: &str, now: u64) {
        if let Some(run) = self.runs.get_mut(binding)
            && matches!(run.program.mode, MacroMode::WhileHeld | MacroMode::Sequence)
            && !matches!(run.phase, Phase::Finish)
        {
            run.finish(now);
        }
    }

    /// Advance every due run, returning bounded ordered input batches.
    pub fn advance(&mut self, now: u64) -> Vec<Playback> {
        let mut output = Vec::new();
        self.runs.retain(|_, run| run.advance(now, &mut output));
        output
    }

    /// Cancel one binding immediately, skipping its finish timeline.
    pub fn cancel(&mut self, binding: &str) -> Vec<Playback> {
        self.runs
            .remove(binding)
            .map_or_else(Vec::new, |run| vec![Playback::Cleanup(run.owner)])
    }

    /// Cancel the run whose injector failed, preserving unrelated macros.
    pub fn cancel_owner(&mut self, owner: u64) -> Vec<Playback> {
        let binding = self
            .runs
            .iter()
            .find(|(_, run)| run.owner == owner)
            .map(|(binding, _)| binding.clone());
        binding.map_or_else(Vec::new, |binding| self.cancel(&binding))
    }

    /// Whether the output worker can sleep until its next command.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.runs.is_empty()
    }

    /// Emergency cancellation: no user-programmed delay or finish can defer it.
    pub fn cancel_all(&mut self) -> Vec<Playback> {
        std::mem::take(&mut self.runs)
            .into_values()
            .map(|run| Playback::Cleanup(run.owner))
            .collect()
    }

    /// Snapshot for GUI status.
    #[must_use]
    pub fn running(&self) -> BTreeMap<String, String> {
        self.runs
            .iter()
            .map(|(binding, run)| (binding.clone(), run.macro_id.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::MacroInput;
    #[test]
    fn example_outputs_exact_digits_once_or_three_times() {
        for (mode, expected) in [
            (MacroMode::Once, "12345"),
            (MacroMode::Repeat(3), "123451234512345"),
        ] {
            let mut program = MacroDefinition::example();
            program.mode = mode;
            let mut scheduler = MacroScheduler::default();
            scheduler.press("g4", "example", &program, 0).unwrap();
            let mut digits = String::new();
            for now in 0..2000 {
                for step in scheduler.advance(now) {
                    if let Playback::Event(_, MacroEvent::Down(MacroInput::Key { scan_code, .. })) =
                        step
                    {
                        digits.push(char::from_digit(u32::from(scan_code - 1), 10).unwrap());
                    }
                }
            }
            assert_eq!(digits, expected);
            assert!(scheduler.is_idle());
        }
    }
    #[test]
    fn sequence_orders_setup_body_and_finish_once() {
        let mut p = MacroDefinition::example();
        p.mode = MacroMode::Sequence;
        p.start = vec![MacroEvent::Down(MacroInput::Right)];
        p.cycle = vec![MacroEvent::Wheel { x: 0, y: 120 }, MacroEvent::Delay(5000)];
        p.finish = vec![MacroEvent::Up(MacroInput::Right)];
        let mut scheduler = MacroScheduler::default();
        scheduler.press("g4", "sequence", &p, 0).unwrap();
        assert_eq!(
            scheduler.advance(0),
            [
                Playback::Event(1, MacroEvent::Down(MacroInput::Right)),
                Playback::Event(1, MacroEvent::Wheel { x: 0, y: 120 })
            ]
        );
        scheduler.release("g4", 10);
        scheduler.release("g4", 11);
        assert_eq!(
            scheduler.advance(11),
            [
                Playback::Event(1, MacroEvent::Up(MacroInput::Right)),
                Playback::Cleanup(1)
            ]
        );
        assert!(scheduler.advance(5000).is_empty());
    }
    fn key() -> MacroInput {
        MacroInput::Key {
            scan_code: 2,
            extended: false,
        }
    }
    fn program(mode: MacroMode) -> MacroDefinition {
        let mut p = MacroDefinition::example();
        p.mode = mode;
        p.cycle = vec![
            MacroEvent::Down(key()),
            MacroEvent::Delay(5000),
            MacroEvent::Up(key()),
        ];
        p
    }
    #[test]
    fn hold_release_preempts_long_delay_and_runs_finish_once() {
        let mut p = program(MacroMode::Sequence);
        p.finish = vec![MacroEvent::Wheel { x: 0, y: 120 }];
        let mut scheduler = MacroScheduler::default();
        scheduler.press("g4", "test", &p, 0).unwrap();
        assert_eq!(
            scheduler.advance(0),
            [Playback::Event(1, MacroEvent::Down(key()))]
        );
        scheduler.release("g4", 20);
        assert_eq!(
            scheduler.advance(20),
            [
                Playback::Event(1, MacroEvent::Wheel { x: 0, y: 120 }),
                Playback::Cleanup(1)
            ]
        );
        assert!(scheduler.advance(5000).is_empty());
    }
    #[test]
    fn emergency_stop_skips_finish_and_all_future_input() {
        let mut p = program(MacroMode::Toggle);
        p.finish = vec![MacroEvent::Down(MacroInput::Right), MacroEvent::Delay(5000)];
        let mut scheduler = MacroScheduler::default();
        scheduler.press("g4", "test", &p, 0).unwrap();
        scheduler.advance(0);
        assert_eq!(scheduler.cancel_all(), [Playback::Cleanup(1)]);
        assert!(scheduler.advance(10000).is_empty());
    }
    #[test]
    fn finite_repeat_is_exact_and_running_retrigger_is_ignored() {
        let mut p = program(MacroMode::Repeat(3));
        p.cycle = vec![MacroEvent::Down(key()), MacroEvent::Up(key())];
        let mut scheduler = MacroScheduler::default();
        scheduler.press("g4", "test", &p, 0).unwrap();
        let first = scheduler.advance(0);
        scheduler.press("g4", "test", &p, 10).unwrap();
        assert!(scheduler.advance(49).is_empty());
        let second = scheduler.advance(50);
        let third = scheduler.advance(100);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_eq!(
            third,
            [
                Playback::Event(1, MacroEvent::Down(key())),
                Playback::Event(1, MacroEvent::Up(key())),
                Playback::Cleanup(1)
            ]
        );
        assert!(scheduler.running().is_empty());
    }
    #[test]
    fn different_bindings_run_in_parallel_and_cancel_independently() {
        let mut scheduler = MacroScheduler::default();
        let p = program(MacroMode::Toggle);
        scheduler.press("g4", "test", &p, 0).unwrap();
        scheduler.press("g5", "test", &p, 0).unwrap();
        assert_eq!(scheduler.advance(0).len(), 2);
        assert_eq!(scheduler.cancel("g4"), [Playback::Cleanup(1)]);
        assert_eq!(scheduler.running().len(), 1);
        scheduler.press("g5", "test", &p, 10).unwrap();
        assert_eq!(scheduler.advance(10), [Playback::Cleanup(2)]);
    }
}
