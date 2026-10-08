//! Scan-code SendInput adapter. OS-visible holds have one shared authority.
#![expect(
    unsafe_code,
    reason = "SendInput and MapVirtualKeyW are native input APIs"
)]

use super::MacroOutputError;
use openlogi_core::macros::{InputOwnership, MacroEvent, MacroInput};
use std::collections::BTreeMap;
use std::sync::{
    LazyLock, Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput,
};

static OUTPUT: LazyLock<Mutex<InputOwnership>> =
    LazyLock::new(|| Mutex::new(InputOwnership::default()));
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
static LEGACY: LazyLock<Mutex<BTreeMap<MacroInput, usize>>> = LazyLock::new(Mutex::default);
const EVENT_TAG: usize = 0x4f4c_4d43;

pub(super) fn next_owner() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn scan_for_vk(vk: u16) -> Option<MacroInput> {
    // SAFETY: accepts a virtual-key value and reads the current keyboard layout.
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) };
    let scan_code = u16::try_from(mapped & 0xff).ok()?;
    (scan_code != 0).then_some(MacroInput::Key {
        scan_code,
        extended: mapped & 0xff00 == 0xe000,
    })
}

pub(super) fn post(owner: u64, event: &MacroEvent) -> Result<(), MacroOutputError> {
    let mut ledger = OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    match event {
        MacroEvent::Down(input) => {
            if ledger.press(owner, *input) {
                send(&[edge(*input, false)])?;
            }
        }
        MacroEvent::Up(input) => {
            if ledger.release(owner, *input)
                && let Err(error) = send(&[edge(*input, true)])
            {
                ledger.press(owner, *input);
                return Err(error);
            }
        }
        MacroEvent::Wheel { x, y } => {
            let mut inputs = Vec::new();
            if *x != 0 {
                inputs.push(mouse(MOUSEEVENTF_HWHEEL, *x));
            }
            if *y != 0 {
                inputs.push(mouse(MOUSEEVENTF_WHEEL, *y));
            }
            send(&inputs)?;
        }
        MacroEvent::Delay(_) => return Err(MacroOutputError("Delay sent to injector".into())),
    }
    Ok(())
}

pub(super) fn cleanup(owner: u64) -> Result<(), MacroOutputError> {
    let mut ledger = OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    let mut failure = None;
    for input in ledger.release_all(owner) {
        if let Err(error) = send(&[edge(input, true)]) {
            ledger.press(owner, input);
            failure = Some(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// Legacy held chords retain their aggregate reference counts and share one
/// output lease (owner zero) with independently owned macro runs.
pub(crate) fn legacy_transition(up: &[MacroInput], down: &[MacroInput]) {
    let mut aliases = LEGACY.lock().unwrap_or_else(PoisonError::into_inner);
    let (up, down) = legacy_edges(&mut aliases, up, down);
    let mut ledger = OUTPUT.lock().unwrap_or_else(PoisonError::into_inner);
    let mut inputs = Vec::new();
    for input in up {
        if ledger.release(0, input) {
            inputs.push(edge(input, true));
        }
    }
    for input in down {
        if ledger.press(0, input) {
            inputs.push(edge(input, false));
        }
    }
    if let Err(error) = send(&inputs) {
        tracing::warn!(%error, "held shortcut injection failed");
    }
}

fn legacy_edges(
    counts: &mut BTreeMap<MacroInput, usize>,
    up: &[MacroInput],
    down: &[MacroInput],
) -> (Vec<MacroInput>, Vec<MacroInput>) {
    let before = counts.clone();
    for input in down {
        *counts.entry(*input).or_default() += 1;
    }
    for input in up {
        if let Some(count) = counts.get_mut(input) {
            *count -= 1;
            if *count == 0 {
                counts.remove(input);
            }
        }
    }
    (
        before
            .keys()
            .filter(|input| !counts.contains_key(input))
            .copied()
            .collect(),
        counts
            .keys()
            .filter(|input| !before.contains_key(input))
            .copied()
            .collect(),
    )
}

fn edge(input: MacroInput, up: bool) -> INPUT {
    match input {
        MacroInput::Key {
            scan_code,
            extended,
        } => INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan_code,
                    dwFlags: KEYEVENTF_SCANCODE
                        | if up { KEYEVENTF_KEYUP } else { 0 }
                        | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
                    time: 0,
                    dwExtraInfo: EVENT_TAG,
                },
            },
        },
        MacroInput::Left => mouse(
            if up {
                MOUSEEVENTF_LEFTUP
            } else {
                MOUSEEVENTF_LEFTDOWN
            },
            0,
        ),
        MacroInput::Right => mouse(
            if up {
                MOUSEEVENTF_RIGHTUP
            } else {
                MOUSEEVENTF_RIGHTDOWN
            },
            0,
        ),
        MacroInput::Middle => mouse(
            if up {
                MOUSEEVENTF_MIDDLEUP
            } else {
                MOUSEEVENTF_MIDDLEDOWN
            },
            0,
        ),
        MacroInput::Back => mouse(
            if up {
                MOUSEEVENTF_XUP
            } else {
                MOUSEEVENTF_XDOWN
            },
            1,
        ),
        MacroInput::Forward => mouse(
            if up {
                MOUSEEVENTF_XUP
            } else {
                MOUSEEVENTF_XDOWN
            },
            2,
        ),
    }
}

fn mouse(flags: u32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data.cast_unsigned(),
                dwFlags: flags,
                time: 0,
                dwExtraInfo: EVENT_TAG,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), MacroOutputError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let count = u32::try_from(inputs.len())
        .map_err(|_| MacroOutputError("Input batch too large".into()))?;
    let size = i32::try_from(size_of::<INPUT>())
        .map_err(|_| MacroOutputError("Invalid INPUT size".into()))?;
    // SAFETY: initialized slice remains alive for the call, with the exact size.
    let accepted = unsafe { SendInput(count, inputs.as_ptr(), size) };
    if accepted != count {
        return Err(MacroOutputError(format!(
            "SendInput accepted {accepted}/{count} events; check target permissions"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "native SendInput acceptance; run explicitly with an idle focused test window"]
    fn native_macros_and_hold_shortcut_share_a_real_held_key() {
        use crate::macro_output::MacroOutput;
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        fn held() -> bool {
            // SAFETY: reads the native F13 state without mutating it.
            unsafe { GetAsyncKeyState(0x7c) < 0 }
        }
        fn check(expected: bool) {
            for _ in 0..20 {
                if held() == expected {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(held(), expected);
        }
        assert!(!held(), "F13 must be idle before acceptance");
        let key = scan_for_vk(0x7c).unwrap();
        let mut first = MacroOutput::default();
        let mut second = MacroOutput::default();
        first.post(1, &MacroEvent::Down(key)).unwrap();
        check(true);
        let legacy = crate::press_hold(&"F13".parse().unwrap());
        second.post(1, &MacroEvent::Down(key)).unwrap();
        first.cleanup(1).unwrap();
        check(true);
        drop(legacy);
        check(true);
        second.cleanup_all().unwrap();
        check(false);
    }
    #[test]
    fn two_legacy_aliases_share_the_same_physical_key() {
        let key = MacroInput::Key {
            scan_code: 0x1d,
            extended: false,
        };
        let mut counts = BTreeMap::new();
        assert_eq!(legacy_edges(&mut counts, &[], &[key]), (vec![], vec![key]));
        assert_eq!(legacy_edges(&mut counts, &[], &[key]), (vec![], vec![]));
        assert_eq!(legacy_edges(&mut counts, &[key], &[]), (vec![], vec![]));
        assert_eq!(legacy_edges(&mut counts, &[key], &[]), (vec![key], vec![]));
    }
    #[test]
    fn scan_edges_preserve_extended_key_and_injection_tag() {
        let input = edge(
            MacroInput::Key {
                scan_code: 0x1d,
                extended: true,
            },
            true,
        );
        // SAFETY: edge's Key arm initializes ki.
        let key = unsafe { input.Anonymous.ki };
        assert_eq!(key.wScan, 0x1d);
        assert_eq!(key.wVk, 0);
        assert_eq!(
            key.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        assert_eq!(key.dwExtraInfo, EVENT_TAG);
    }
}
