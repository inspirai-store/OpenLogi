//! Native macro library, timeline editor, recording and G604 calibration wizard.
use crate::services::ipc::{G604Control, MacroControl, MacroState};
use crate::state::AppState;
use crate::ui::theme::{self, Typography as _};
use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div,
};
use gpui_component::{
    Disableable as _, Selectable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    v_flex,
};
use openlogi_core::binding::Action;
use openlogi_core::macros::g604::{BUTTONS, G604Command, G604Status};
use openlogi_core::macros::{
    MacroCommand, MacroDefinition, MacroEvent, MacroImport, MacroInput, MacroMode, MacroStatus,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retained editor state; hardware and recording remain in the agent.
pub struct MacroPanel {
    selected: Option<String>,
    draft: MacroDefinition,
    phase: usize,
    row: Option<usize>,
    page: usize,
    row_ids: [Vec<usize>; 3],
    next_row: usize,
    name: Entity<InputState>,
    delay: Entity<InputState>,
    repeats: Entity<InputState>,
    scan: Entity<InputState>,
    wheel: Entity<InputState>,
    emergency: Entity<InputState>,
    test: Entity<InputState>,
    input_kind: usize,
    extended: bool,
    status: MacroStatus,
    g604: G604Status,
    imports: Vec<MacroImport>,
    message: Option<String>,
    busy: bool,
    visible: bool,
    _poll: gpui::Task<()>,
}
fn input(value: &str, window: &mut Window, cx: &mut Context<MacroPanel>) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).default_value(value))
}
fn events(def: &MacroDefinition, phase: usize) -> &Vec<MacroEvent> {
    match phase {
        0 => &def.start,
        1 => &def.cycle,
        _ => &def.finish,
    }
}
fn events_mut(def: &mut MacroDefinition, phase: usize) -> &mut Vec<MacroEvent> {
    match phase {
        0 => &mut def.start,
        1 => &mut def.cycle,
        _ => &mut def.finish,
    }
}
fn input_label(input: MacroInput) -> String {
    match input {
        MacroInput::Key {
            scan_code,
            extended,
        } => {
            let name = if (2..=11).contains(&scan_code) {
                ((scan_code - 1) % 10).to_string()
            } else {
                format!("0x{scan_code:02X}")
            };
            tr!(
                "macros.key",
                key = name,
                extended = if extended { "E0" } else { "" }
            )
            .to_string()
        }
        MacroInput::Left => tr!("macros.left").into(),
        MacroInput::Right => tr!("macros.right").into(),
        MacroInput::Middle => tr!("macros.middle").into(),
        MacroInput::Back => tr!("macros.back").into(),
        MacroInput::Forward => tr!("macros.forward").into(),
    }
}
fn event_label(event: &MacroEvent) -> String {
    match event {
        MacroEvent::Down(input) => tr!("macros.down", input = input_label(*input)).into(),
        MacroEvent::Up(input) => tr!("macros.up", input = input_label(*input)).into(),
        MacroEvent::Delay(ms) => tr!("macros.delay_value", ms = ms).into(),
        MacroEvent::Wheel { x, y } => tr!("macros.wheel_value", x = x, y = y).into(),
    }
}
impl MacroPanel {
    /// Build stable input entities and the cached-state polling task.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |panel, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let pending = panel.update(cx, |panel, cx| {
                    if !panel.visible {
                        return None;
                    }
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    if AppState::try_read(cx)
                        .is_some_and(|state| state.macro_request(MacroState { reply }))
                    {
                        Some(rx)
                    } else {
                        None
                    }
                });
                let Ok(pending) = pending else {
                    break;
                };
                if let Some(rx) = pending
                    && let Ok(result) = rx.await
                    && panel
                        .update(cx, |panel, cx| {
                            match result {
                                Ok((status, g604)) => {
                                    panel.status = status;
                                    panel.g604 = g604;
                                }
                                Err(error) => panel.message = Some(error),
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
            }
        });
        let emergency = AppState::try_read(cx).map_or_else(
            || "ctrl+alt+f12".into(),
            |s| s.macro_config().emergency_stop.to_string(),
        );
        let mut panel = Self {
            selected: None,
            draft: MacroDefinition::example(),
            phase: 1,
            row: None,
            page: 0,
            row_ids: [Vec::new(), Vec::new(), Vec::new()],
            next_row: 0,
            name: input("12345", window, cx),
            delay: input("50", window, cx),
            repeats: input("3", window, cx),
            scan: input("2", window, cx),
            wheel: input("0,120", window, cx),
            emergency: input(&emergency, window, cx),
            test: input("", window, cx),
            input_kind: 0,
            extended: false,
            status: MacroStatus::default(),
            g604: G604Status::default(),
            imports: vec![],
            message: None,
            busy: false,
            visible: false,
            _poll: poll,
        };
        panel.rebuild_ids();
        panel
    }
    /// Only poll while this workspace is visible.
    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            cx.notify();
        }
    }
    fn rebuild_ids(&mut self) {
        for phase in 0..3 {
            self.row_ids[phase] = (0..events(&self.draft, phase).len())
                .map(|_| {
                    self.next_row += 1;
                    self.next_row
                })
                .collect();
        }
    }
    fn load(
        &mut self,
        id: Option<String>,
        definition: MacroDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = id;
        self.name.update(cx, |name, cx| {
            name.set_value(definition.name.clone(), window, cx);
        });
        if let MacroMode::Repeat(count) = definition.mode {
            self.repeats.update(cx, |input, cx| {
                input.set_value(count.to_string(), window, cx);
            });
        }
        self.delay.update(cx, |input, cx| {
            input.set_value(definition.interval_ms.to_string(), window, cx);
        });
        self.draft = definition;
        self.row = None;
        self.page = 0;
        self.rebuild_ids();
        self.message = None;
        cx.notify();
    }
    fn program(&self, cx: &gpui::App) -> Result<MacroDefinition, String> {
        let mut def = self.draft.clone();
        def.name = self.name.read(cx).value().to_string();
        if matches!(def.mode, MacroMode::Repeat(_)) {
            def.mode = MacroMode::Repeat(
                self.repeats
                    .read(cx)
                    .value()
                    .parse()
                    .map_err(|_| tr!("macros.invalid_count").to_string())?,
            );
        }
        def.validate().map_err(|e| e.to_string())?;
        Ok(def)
    }
    fn save(&mut self, cx: &mut Context<Self>) {
        let result = self.program(cx).and_then(|definition| {
            let id = self.selected.clone().unwrap_or_else(|| {
                format!(
                    "macro-{}",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                )
            });
            AppState::update(cx, |state, _| {
                state.commit_macro(id.clone(), definition.clone())
            })?;
            self.selected = Some(id);
            self.draft = definition;
            Ok(())
        });
        self.message = Some(result.map_or_else(|e| e, |()| tr!("macros.saved").into()));
        cx.notify();
    }
    fn control(&mut self, command: MacroCommand, window: &mut Window, cx: &mut Context<Self>) {
        let stop_recording = matches!(command, MacroCommand::StopRecording);
        let (reply, rx) = tokio::sync::oneshot::channel();
        if !AppState::try_read(cx)
            .is_some_and(|state| state.macro_request(MacroControl { command, reply }))
        {
            self.message = Some(tr!("macros.agent_unavailable").into());
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |panel, cx| {
            let result = rx.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = panel.update_in(cx, |panel, window, cx| {
                match result {
                    Ok(reply) => {
                        panel.imports = reply.imports;
                        panel.message = None;
                        if stop_recording {
                            let (reply, rx) = tokio::sync::oneshot::channel();
                            if AppState::try_read(cx)
                                .is_some_and(|state| state.macro_request(MacroState { reply }))
                            {
                                cx.spawn_in(window, async move |panel, cx| {
                                    if let Ok(Ok((status, _))) = rx.await {
                                        let _ = panel.update_in(cx, |panel, window, cx| {
                                            let mut def = panel.draft.clone();
                                            def.name = panel.name.read(cx).value().to_string();
                                            *events_mut(&mut def, panel.phase) = status.recorded;
                                            panel.load(panel.selected.clone(), def, window, cx);
                                        });
                                    }
                                })
                                .detach();
                            }
                        }
                    }
                    Err(error) => panel.message = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn hardware(&mut self, command: G604Command, cx: &mut Context<Self>) {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.busy = true;
        self.message = None;
        if !AppState::try_read(cx)
            .is_some_and(|state| state.macro_request(G604Control { command, reply }))
        {
            self.busy = false;
            self.message = Some(tr!("macros.agent_unavailable").into());
            return;
        }
        cx.spawn(async move |panel, cx| {
            let result = rx.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = panel.update(cx, |panel, cx| {
                panel.busy = false;
                match result {
                    Ok(status) => panel.g604 = status,
                    Err(error) => panel.message = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn value_ms(&self, cx: &gpui::App) -> Result<u64, String> {
        let value = self
            .delay
            .read(cx)
            .value()
            .parse::<u64>()
            .map_err(|_| tr!("macros.invalid_delay").to_string())?;
        if value > openlogi_core::macros::MAX_DELAY_MS {
            return Err(tr!("macros.invalid_delay").into());
        }
        Ok(value)
    }
    fn chosen_input(&self, cx: &gpui::App) -> Result<MacroInput, String> {
        Ok(match self.input_kind {
            0 => {
                let scan_code = self
                    .scan
                    .read(cx)
                    .value()
                    .parse::<u16>()
                    .map_err(|_| tr!("macros.invalid_key").to_string())?;
                if !(1..=255).contains(&scan_code) {
                    return Err(tr!("macros.invalid_key").into());
                }
                MacroInput::Key {
                    scan_code,
                    extended: self.extended,
                }
            }
            1 => MacroInput::Left,
            2 => MacroInput::Right,
            3 => MacroInput::Middle,
            4 => MacroInput::Back,
            _ => MacroInput::Forward,
        })
    }
    fn insert(&mut self, values: Vec<MacroEvent>, cx: &mut Context<Self>) {
        let at = self
            .row
            .map_or_else(|| events(&self.draft, self.phase).len(), |i| i + 1);
        for (offset, event) in values.into_iter().enumerate() {
            events_mut(&mut self.draft, self.phase).insert(at + offset, event);
            self.next_row += 1;
            self.row_ids[self.phase].insert(at + offset, self.next_row);
        }
        self.row = Some(at);
        self.page = at / 40;
        cx.notify();
    }
    fn edit_row(&mut self, action: usize, cx: &mut Context<Self>) {
        let Some(row) = self.row else {
            return;
        };
        let items = events_mut(&mut self.draft, self.phase);
        if row >= items.len() {
            return;
        }
        match action {
            0 => {
                items.remove(row);
                self.row_ids[self.phase].remove(row);
                self.row = None;
            }
            1 => {
                let event = items[row].clone();
                self.insert(vec![event], cx);
            }
            2 | 3 => {
                let target = if action == 2 {
                    row.checked_sub(1)
                } else {
                    Some(row + 1)
                };
                if let Some(target) = target
                    && target < items.len()
                {
                    items.swap(row, target);
                    self.row_ids[self.phase].swap(row, target);
                    self.row = Some(target);
                    self.page = target / 40;
                }
            }
            _ => {}
        }
        cx.notify();
    }
    fn edit_value(&mut self, hold: bool, cx: &mut Context<Self>) {
        let result = (|| {
            let row = self
                .row
                .ok_or_else(|| tr!("macros.select_event").to_string())?;
            let old = events(&self.draft, self.phase)
                .get(row)
                .cloned()
                .ok_or("Missing event")?;
            if hold {
                if !matches!(old, MacroEvent::Down(_)) {
                    return Err(tr!("macros.select_down").to_string());
                }
                let delay = self.value_ms(cx)?;
                openlogi_core::macros::set_hold_duration(
                    events_mut(&mut self.draft, self.phase),
                    row,
                    delay,
                )
                .map_err(|error| error.to_string())?;
                self.rebuild_ids();
            } else {
                let event = match old {
                    MacroEvent::Down(_) => MacroEvent::Down(self.chosen_input(cx)?),
                    MacroEvent::Up(_) => MacroEvent::Up(self.chosen_input(cx)?),
                    MacroEvent::Delay(_) => MacroEvent::Delay(self.value_ms(cx)?),
                    MacroEvent::Wheel { .. } => {
                        let value = self.wheel.read(cx).value();
                        let (x, y) = value
                            .split_once(',')
                            .ok_or_else(|| tr!("macros.invalid_wheel").to_string())?;
                        MacroEvent::Wheel {
                            x: x.trim()
                                .parse()
                                .map_err(|_| tr!("macros.invalid_wheel").to_string())?,
                            y: y.trim()
                                .parse()
                                .map_err(|_| tr!("macros.invalid_wheel").to_string())?,
                        }
                    }
                };
                events_mut(&mut self.draft, self.phase)[row] = event;
            }
            Ok::<_, String>(())
        })();
        self.message = result.err();
        cx.notify();
    }
    fn file(&mut self, export: bool, window: &mut Window, cx: &mut Context<Self>) {
        if export {
            let Ok(program) = self.program(cx) else {
                self.message = Some(tr!("macros.invalid_program").into());
                return;
            };
            let directory = std::env::var_os("USERPROFILE")
                .map_or_else(|| ".".into(), std::path::PathBuf::from);
            let prompt = cx.prompt_for_new_path(&directory, Some("macro.json"));
            cx.spawn(async move |panel, cx| {
                if let Ok(Ok(Some(path))) = prompt.await {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            serde_json::to_vec_pretty(&program)
                                .map_err(|e| e.to_string())
                                .and_then(|bytes| {
                                    std::fs::write(path, bytes).map_err(|e| e.to_string())
                                })
                        })
                        .await;
                    let _ = panel.update(cx, |panel, cx| {
                        panel.message = result.err();
                        cx.notify();
                    });
                }
            })
            .detach();
        } else {
            let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(tr!("macros.import_file")),
            });
            cx.spawn_in(window, async move |panel, cx| {
                if let Ok(Ok(Some(paths))) = prompt.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            if std::fs::metadata(&path).map_err(|e| e.to_string())?.len()
                                > 16_000_000
                            {
                                return Err("Macro file exceeds 16 MB".into());
                            }
                            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                            let def: MacroDefinition =
                                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                            def.validate().map_err(|e| e.to_string())?;
                            Ok::<_, String>(def)
                        })
                        .await;
                    let _ = panel.update_in(cx, |panel, window, cx| match result {
                        Ok(def) => panel.load(None, def, window, cx),
                        Err(error) => {
                            panel.message = Some(error);
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }
}
impl MacroPanel {
    fn render_library(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let library = AppState::try_read(cx)
            .map(|state| {
                state
                    .macro_config()
                    .library
                    .iter()
                    .map(|(id, def)| (id.clone(), def.name.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let list = v_flex()
            .gap_1()
            .children(library.into_iter().map(|(id, name)| {
                let selected = self.selected.as_ref() == Some(&id);
                Button::new(SharedString::from(format!("macro-library-{id}")))
                    .label(name)
                    .selected(selected)
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        let def = AppState::try_read(cx)
                            .and_then(|state| state.macro_config().library.get(&id))
                            .cloned();
                        if let Some(def) = def {
                            panel.load(Some(id.clone()), def, window, cx);
                        }
                    }))
            }));
        list.into_any_element()
    }
    fn render_rows(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let items = events(&self.draft, self.phase);
        let total = items.len();
        let start = (self.page * 40).min(total);
        let rows = v_flex()
            .gap_1()
            .children(
                items
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(40)
                    .map(|(row, event)| {
                        Button::new(SharedString::from(format!(
                            "macro-event-{}",
                            self.row_ids[self.phase][row]
                        )))
                        .label(format!("{} · {}", row + 1, event_label(event)))
                        .selected(self.row == Some(row))
                        .on_click(cx.listener(
                            move |panel, _, window, cx| {
                                panel.row = Some(row);
                                panel.select_event(row, window, cx);
                                cx.notify();
                            },
                        ))
                    }),
            );
        rows.into_any_element()
    }
    fn render_modes(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let modes = [
            (MacroMode::Once, tr!("macros.once")),
            (MacroMode::Repeat(3), tr!("macros.repeat")),
            (MacroMode::WhileHeld, tr!("macros.while_held")),
            (MacroMode::Toggle, tr!("macros.toggle")),
            (MacroMode::Sequence, tr!("macros.sequence")),
        ];
        let mode_buttons =
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(modes.into_iter().enumerate().map(|(i, (mode, label))| {
                    Button::new(("macro-mode", i))
                        .label(label)
                        .selected(
                            std::mem::discriminant(&self.draft.mode)
                                == std::mem::discriminant(&mode),
                        )
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.draft.mode = mode;
                            cx.notify();
                        }))
                }));
        mode_buttons.into_any_element()
    }
    fn render_phases(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let phases = h_flex().gap_2().children(
            [
                tr!("macros.start"),
                tr!("macros.cycle"),
                tr!("macros.finish"),
            ]
            .into_iter()
            .enumerate()
            .map(|(phase, label)| {
                Button::new(("macro-phase", phase))
                    .label(label)
                    .selected(self.phase == phase)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.phase = phase;
                        panel.row = None;
                        panel.page = 0;
                        cx.notify();
                    }))
            }),
        );
        phases.into_any_element()
    }
    fn render_recorder(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let recorder = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("macro-record")
                    .label(if self.status.recording {
                        tr!("macros.stop_recording")
                    } else {
                        tr!("macros.record")
                    })
                    .on_click(cx.listener(|panel, _, window, cx| {
                        panel.control(
                            if panel.status.recording {
                                MacroCommand::StopRecording
                            } else {
                                MacroCommand::Record
                            },
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new("macro-play")
                    .label(tr!("macros.test"))
                    .on_click(cx.listener(|panel, _, window, cx| match panel.program(cx) {
                        Ok(definition) => {
                            panel.control(
                                MacroCommand::Play {
                                    definition,
                                    delay_ms: 1500,
                                },
                                window,
                                cx,
                            );
                            panel.test.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                                input.focus(window, cx);
                            });
                        }
                        Err(error) => {
                            panel.message = Some(error);
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("macro-release")
                    .label(tr!("macros.release"))
                    .on_click(cx.listener(|panel, _, window, cx| {
                        panel.control(MacroCommand::Release("preview".into()), window, cx);
                    })),
            )
            .child(
                Button::new("macro-stop")
                    .label(tr!("macros.stop_all"))
                    .on_click(cx.listener(|panel, _, window, cx| {
                        panel.control(MacroCommand::StopAll, window, cx);
                    })),
            );
        recorder.into_any_element()
    }
    fn render_edit(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let edit = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("macro-add-key")
                    .label(tr!("macros.add_press"))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        match panel
                            .chosen_input(cx)
                            .and_then(|input| Ok((input, panel.value_ms(cx)?)))
                        {
                            Ok((input, ms)) => panel.insert(
                                vec![
                                    MacroEvent::Down(input),
                                    MacroEvent::Delay(ms),
                                    MacroEvent::Up(input),
                                ],
                                cx,
                            ),
                            Err(error) => {
                                panel.message = Some(error);
                                cx.notify();
                            }
                        }
                    })),
            )
            .child(
                Button::new("macro-add-delay")
                    .label(tr!("macros.add_delay"))
                    .on_click(cx.listener(|panel, _, _, cx| match panel.value_ms(cx) {
                        Ok(ms) => panel.insert(vec![MacroEvent::Delay(ms)], cx),
                        Err(e) => panel.message = Some(e),
                    })),
            )
            .child(
                Button::new("macro-add-wheel")
                    .label(tr!("macros.add_wheel"))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.insert(vec![MacroEvent::Wheel { x: 0, y: 120 }], cx);
                    })),
            )
            .children(
                [
                    tr!("macros.delete"),
                    tr!("common.copy"),
                    tr!("common.up"),
                    tr!("common.down"),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, label)| {
                    Button::new(("macro-edit", i))
                        .label(label)
                        .disabled(self.row.is_none())
                        .on_click(cx.listener(move |panel, _, _, cx| panel.edit_row(i, cx)))
                }),
            );
        edit.into_any_element()
    }
    fn render_bindings(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let bindings = h_flex()
            .gap_2()
            .flex_wrap()
            .children(BUTTONS.into_iter().enumerate().map(|(i, button)| {
                Button::new(("macro-bind", i))
                    .label(tr!("macros.bind", button = button.label()))
                    .disabled(self.selected.is_none())
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if let Some(id) = panel.selected.clone() {
                            AppState::apply(cx, |state| {
                                state.commit_binding(button, Action::RunMacro(id))
                            });
                            panel.message =
                                Some(tr!("macros.bound", button = button.label()).into());
                            cx.notify();
                        }
                    }))
            }));
        bindings.into_any_element()
    }
    fn render_hardware_steps(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .child(h_flex().gap_2().flex_wrap().children((0u8..6).map(|index| {
                Button::new(("g604-calibrate", usize::from(index)))
                    .label(format!(
                        "G{} {}",
                        index + 4,
                        if self.g604.slots[usize::from(index)].is_some() {
                            "✓"
                        } else {
                            ""
                        }
                    ))
                    .disabled(self.busy || !self.g604.recovery_pending || self.g604.prepared)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.hardware(G604Command::Calibrate(index), cx);
                    }))
            })))
            .child(
                Button::new("g604-enable")
                    .label(tr!("macros.write_triggers"))
                    .disabled(
                        self.busy
                            || self.g604.prepared
                            || self.g604.slots.iter().any(Option::is_none),
                    )
                    .on_click(
                        cx.listener(|panel, _, _, cx| panel.hardware(G604Command::Enable, cx)),
                    ),
            )
            .child(h_flex().gap_2().flex_wrap().children((0u8..6).map(|index| {
                Button::new(("g604-verify", usize::from(index)))
                    .label(format!(
                        "G{} → F{} {}",
                        index + 4,
                        index + 13,
                        if self.g604.verified[usize::from(index)] {
                            "✓"
                        } else {
                            ""
                        }
                    ))
                    .disabled(self.busy || !self.g604.prepared || self.g604.enabled)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.hardware(G604Command::Verify(index), cx);
                    }))
            })))
            .child(
                Button::new("g604-accept")
                    .label(if self.g604.enabled {
                        tr!("macros.enabled")
                    } else {
                        tr!("macros.accept")
                    })
                    .disabled(
                        self.busy || self.g604.enabled || !self.g604.verified.iter().all(|v| *v),
                    )
                    .on_click(
                        cx.listener(|panel, _, _, cx| panel.hardware(G604Command::Accept, cx)),
                    ),
            )
            .into_any_element()
    }
    fn render_hardware(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pal = theme::palette(cx);
        let hardware = v_flex()
            .gap_2()
            .child(div().text_heading().child(tr!("macros.g604")))
            .child(
                div()
                    .w_full()
                    .whitespace_normal()
                    .text_body()
                    .child(tr!("macros.calibration_help")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("g604-begin")
                            .label(tr!("macros.backup"))
                            .disabled(
                                self.busy
                                    || self.g604.recovery_pending
                                    || !cfg!(target_os = "windows"),
                            )
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.hardware(G604Command::Begin, cx);
                            })),
                    )
                    .child(
                        Button::new("g604-restore")
                            .label(tr!("macros.restore"))
                            .disabled(self.busy || !self.g604.recovery_pending)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.hardware(G604Command::Restore, cx);
                            })),
                    ),
            )
            .child(self.render_hardware_steps(cx))
            .children(self.g604.awaiting.map(|index| {
                div().child(tr!(
                    "macros.press_button",
                    button = format!("G{}", index + 4)
                ))
            }))
            .children(
                self.g604
                    .error
                    .clone()
                    .map(|error| div().text_color(pal.text_muted).child(error)),
            );
        hardware.into_any_element()
    }
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("macro-new")
                            .label(tr!("macros.new"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.load(None, MacroDefinition::example(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-copy")
                            .label(tr!("common.copy"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                let mut def = panel.draft.clone();
                                def.name =
                                    tr!("macros.copy_name", name = panel.name.read(cx).value())
                                        .into();
                                panel.load(None, def, window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-delete")
                            .label(tr!("macros.delete"))
                            .disabled(self.selected.is_none())
                            .on_click(cx.listener(|panel, _, window, cx| {
                                if let Some(id) = panel.selected.clone() {
                                    let result =
                                        AppState::update(cx, |state, _| state.delete_macro(&id));
                                    panel.load(None, MacroDefinition::example(), window, cx);
                                    panel.message = result.err();
                                }
                            })),
                    )
                    .child(
                        Button::new("macro-import-file")
                            .label(tr!("macros.import_file"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.file(false, window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-export")
                            .label(tr!("macros.export_file"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.file(true, window, cx);
                            })),
                    )
                    .child(
                        Button::new("macro-ghub")
                            .label(tr!("macros.import_ghub"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.control(MacroCommand::ImportGhub, window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
    fn render_imports(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .children(self.imports.iter().enumerate().map(|(i, import)| {
                let def = import.definition.clone();
                v_flex()
                    .gap_1()
                    .child(
                        Button::new(("macro-import-preview", i))
                            .label(tr!(
                                "macros.import_preview",
                                name = &def.name,
                                count = def.start.len() + def.cycle.len() + def.finish.len()
                            ))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.load(None, def.clone(), window, cx);
                            })),
                    )
                    .children(import.unsupported.iter().map(|text| {
                        div()
                            .text_body()
                            .child(tr!("macros.unsupported_action", action = text))
                    }))
            }))
            .into_any_element()
    }
    fn render_program(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mode_buttons = self.render_modes(cx);
        let recorder = self.render_recorder(cx);
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().child(tr!("macros.name")))
                    .flex_wrap()
                    .child(div().w(gpui::px(360.0)).child(Input::new(&self.name)))
                    .child(
                        Button::new("macro-save")
                            .primary()
                            .label(tr!("common.save"))
                            .on_click(cx.listener(|panel, _, _, cx| panel.save(cx))),
                    ),
            )
            .child(mode_buttons)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().child(tr!("macros.repeat_count")))
                    .child(div().w(gpui::px(96.0)).child(Input::new(&self.repeats))),
            )
            .child(recorder)
            .child(div().text_caption().child(tr!("macros.record_help")))
            .children(
                (!cfg!(target_os = "windows"))
                    .then(|| div().text_body().child(tr!("macros.windows_only"))),
            )
            .child(div().text_caption().child(tr!("macros.test_help")))
            .child(Input::new(&self.test))
            .child(self.render_status(cx))
            .into_any_element()
    }
    fn render_status(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let app = self
            .status
            .application
            .as_deref()
            .unwrap_or("—")
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("—");
        let running = self
            .status
            .running
            .iter()
            .map(|(binding, id)| {
                let name = AppState::try_read(cx)
                    .and_then(|state| state.macro_config().library.get(id))
                    .map_or(self.draft.name.as_str(), |def| def.name.as_str());
                format!("{} → {name}", binding.rsplit(':').next().unwrap_or(binding))
            })
            .collect::<Vec<_>>();
        v_flex()
            .gap_1()
            .child(div().text_caption().child(tr!(
                "macros.running",
                count = self.status.running.len(),
                events = self.status.recorded_events,
                app = app
            )))
            .children(
                running
                    .into_iter()
                    .map(|name| div().text_body().child(name)),
            )
            .children(self.status.error.clone().map(|error| div().child(error)))
            .into_any_element()
    }
    fn select_event(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        match events(&self.draft, self.phase).get(row).cloned() {
            Some(MacroEvent::Delay(ms)) => self
                .delay
                .update(cx, |input, cx| input.set_value(ms.to_string(), window, cx)),
            Some(MacroEvent::Wheel { x, y }) => self.wheel.update(cx, |input, cx| {
                input.set_value(format!("{x},{y}"), window, cx);
            }),
            Some(MacroEvent::Down(input) | MacroEvent::Up(input)) => {
                self.input_kind = match input {
                    MacroInput::Key {
                        scan_code,
                        extended,
                    } => {
                        self.extended = extended;
                        self.scan.update(cx, |input, cx| {
                            input.set_value(scan_code.to_string(), window, cx);
                        });
                        0
                    }
                    MacroInput::Left => 1,
                    MacroInput::Right => 2,
                    MacroInput::Middle => 3,
                    MacroInput::Back => 4,
                    MacroInput::Forward => 5,
                };
            }
            None => {}
        }
    }
    fn render_timeline_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let kind = if self.input_kind == 0 {
            tr!("macros.keyboard").to_string()
        } else {
            input_label(
                [
                    MacroInput::Left,
                    MacroInput::Right,
                    MacroInput::Middle,
                    MacroInput::Back,
                    MacroInput::Forward,
                ][self.input_kind - 1],
            )
        };
        let total = events(&self.draft, self.phase).len();
        let edit = self.render_edit(cx);
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("macro-prev-page")
                            .label(tr!("macros.previous"))
                            .disabled(self.page == 0)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.page = panel.page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(div().child(tr!("macros.event_count", count = total)))
                    .child(
                        Button::new("macro-next-page")
                            .label(tr!("macros.next"))
                            .disabled((self.page + 1) * 40 >= total)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.page += 1;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("macro-input-kind")
                            .label(kind)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.input_kind = (panel.input_kind + 1) % 6;
                                cx.notify();
                            })),
                    )
                    .child(div().child(tr!("macros.scan_code")))
                    .child(div().w(gpui::px(96.0)).child(Input::new(&self.scan)))
                    .child(
                        Button::new("macro-extended")
                            .label("E0")
                            .selected(self.extended)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.extended = !panel.extended;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().child(tr!("macros.delay")))
                    .child(div().w(gpui::px(96.0)).child(Input::new(&self.delay)))
                    .child(div().child(tr!("macros.wheel")))
                    .child(div().w(gpui::px(112.0)).child(Input::new(&self.wheel))),
            )
            .child(edit)
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("macro-edit-value")
                            .label(tr!("macros.apply_event"))
                            .on_click(cx.listener(|panel, _, _, cx| panel.edit_value(false, cx))),
                    )
                    .child(
                        Button::new("macro-hold-time")
                            .label(tr!("macros.hold_time"))
                            .on_click(cx.listener(|panel, _, _, cx| panel.edit_value(true, cx))),
                    )
                    .child(
                        Button::new("macro-uniform")
                            .label(tr!("macros.uniform_delay"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                if let Ok(ms) = panel.value_ms(cx) {
                                    panel.draft.set_uniform_delay(ms);
                                    panel.draft.interval_ms = ms;
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .into_any_element()
    }
    fn render_emergency(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().child(tr!("macros.emergency")))
                    .flex_wrap()
                    .child(div().w(gpui::px(300.0)).child(Input::new(&self.emergency)))
                    .child(
                        Button::new("macro-emergency-save")
                            .label(tr!("common.save"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                let result = panel
                                    .emergency
                                    .read(cx)
                                    .value()
                                    .parse()
                                    .map_err(|e: openlogi_core::config::ParseTriggerError| {
                                        e.to_string()
                                    })
                                    .and_then(|chord| {
                                        AppState::update(cx, |state, _| {
                                            state.commit_macro_emergency(chord)
                                        })
                                    });
                                panel.message = result.err();
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}
impl Render for MacroPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .w_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .p_5()
                    .gap_4()
                    .child(div().text_heading().child(tr!("macros.title")))
                    .child(self.render_toolbar(cx))
                    .child(self.render_library(cx))
                    .child(self.render_imports(cx))
                    .child(self.render_program(cx))
                    .child(self.render_phases(cx))
                    .child(self.render_rows(cx))
                    .child(self.render_timeline_controls(cx))
                    .child(div().text_heading().child(tr!("macros.bindings")))
                    .child(self.render_bindings(cx))
                    .child(self.render_emergency(cx))
                    .child(self.render_hardware(cx))
                    .children(
                        self.message
                            .clone()
                            .filter(|message| self.g604.error.as_ref() != Some(message))
                            .map(|message| div().text_body().child(message)),
                    ),
            )
    }
}
