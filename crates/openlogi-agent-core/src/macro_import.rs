//! Read-only G HUB sequence import with explicit unsupported-action previews.
use openlogi_core::macros::{MacroDefinition, MacroEvent, MacroImport, MacroInput, MacroMode};
use serde_json::Value;

/// Read the user's G HUB database in SQLite read-only mode.
pub fn ghub() -> Result<Vec<MacroImport>, String> {
    if !cfg!(target_os = "windows") {
        return Err("G HUB import currently requires Windows".into());
    }
    let path = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?;
    let path = std::path::PathBuf::from(path).join("LGHUB/settings.db");
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
    db.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = db
        .query_row("SELECT file FROM data LIMIT 1", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let cards = value
        .pointer("/cards/cards")
        .and_then(Value::as_array)
        .ok_or("Unknown G HUB cards format")?;
    Ok(cards
        .iter()
        .filter(|card| card.pointer("/macro/type").and_then(Value::as_str) == Some("SEQUENCE"))
        .map(sequence)
        .collect())
}
fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
fn components(sequence: &Value, key: &str, unsupported: &mut Vec<String>) -> Vec<MacroEvent> {
    sequence
        .get(key)
        .and_then(|v| v.get("components"))
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |events| {
            events
                .iter()
                .filter_map(|value| {
                    if let Some(delay) = value.pointer("/delay/durationMs").and_then(number) {
                        return Some(MacroEvent::Delay(delay));
                    }
                    if let Some(key) = value.get("keyboard") {
                        let input = key
                            .get("hidUsage")
                            .and_then(number)
                            .and_then(|usage| u8::try_from(usage).ok())
                            .and_then(|usage| {
                                openlogi_inject::macro_output::input_for_usage(usage).ok()
                            });
                        if let Some(input) = input {
                            return Some(
                                if key.get("isDown").and_then(Value::as_bool).unwrap_or(false) {
                                    MacroEvent::Down(input)
                                } else {
                                    MacroEvent::Up(input)
                                },
                            );
                        }
                    }
                    if let Some(button) = value.pointer("/mouse/button") {
                        let input = match button.get("hidUsage").and_then(number) {
                            Some(1) => Some(MacroInput::Left),
                            Some(2) => Some(MacroInput::Right),
                            Some(3) => Some(MacroInput::Middle),
                            Some(4) => Some(MacroInput::Back),
                            Some(5) => Some(MacroInput::Forward),
                            _ => None,
                        };
                        if let Some(input) = input {
                            return Some(
                                if button
                                    .get("isDown")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                {
                                    MacroEvent::Down(input)
                                } else {
                                    MacroEvent::Up(input)
                                },
                            );
                        }
                    }
                    unsupported.push(value.to_string());
                    None
                })
                .collect()
        })
}
fn sequence(card: &Value) -> MacroImport {
    let sequence = &card["macro"]["sequence"];
    let mut unsupported = vec![];
    let start = components(sequence, "pressSequence", &mut unsupported);
    let finish = components(sequence, "releaseSequence", &mut unsupported);
    let held = components(sequence, "heldSequence", &mut unsupported);
    let toggle = components(sequence, "toggleSequence", &mut unsupported);
    let simple = components(sequence, "simpleSequence", &mut unsupported);
    let (cycle, mode) = if !toggle.is_empty() {
        (toggle, MacroMode::Toggle)
    } else if !held.is_empty() {
        (held, MacroMode::Sequence)
    } else {
        (simple, MacroMode::Once)
    };
    let definition = MacroDefinition {
        name: card["name"].as_str().unwrap_or("G HUB macro").into(),
        mode,
        start,
        cycle,
        finish,
        interval_ms: sequence["defaultDelay"].as_u64().unwrap_or(50),
    };
    if let Err(error) = definition.validate() {
        unsupported.push(error.to_string());
    }
    MacroImport {
        definition,
        unsupported,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ghub_missing_is_down_means_release_and_unknown_actions_are_visible() {
        let card = serde_json::json!({"name":"test","macro":{"sequence":{"toggleSequence":{"components":[{"mouse":{"button":{"hidUsage":"2","isDown":true}}},{"mouse":{"button":{"hidUsage":"2"}}},{"script":"unsupported"}]}}}});
        let result = sequence(&card);
        assert_eq!(
            result.definition.cycle,
            vec![
                MacroEvent::Down(MacroInput::Right),
                MacroEvent::Up(MacroInput::Right)
            ]
        );
        assert_eq!(result.unsupported.len(), 1);
    }
}
