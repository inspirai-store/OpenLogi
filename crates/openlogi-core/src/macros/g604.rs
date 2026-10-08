//! Recovery and calibration data for the G604 Bluetooth trigger profile.
use serde::{Deserialize, Serialize};

/// Six physical side controls; deliberately separate from protocol slot order.
pub const BUTTONS: [crate::binding::ButtonId; 6] = [
    crate::binding::ButtonId::G4,
    crate::binding::ButtonId::G5,
    crate::binding::ButtonId::G6,
    crate::binding::ButtonId::G7,
    crate::binding::ButtonId::G8,
    crate::binding::ButtonId::G9,
];

/// Immutable snapshot written before any onboard memory modification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OnboardBackup {
    /// Exact route of the Bluetooth device.
    pub route: crate::hid::DeviceRoute,
    /// Descriptor bytes and original geometry.
    pub descriptor: Vec<u8>,
    /// Original control mode (1 onboard, 2 host).
    pub mode: u8,
    /// Original selected profile.
    pub profile: u16,
    /// Original current sensor DPI.
    pub dpi: u16,
    /// Complete directory sector.
    pub directory: Vec<u8>,
    /// Complete spare profile sector, including CRC.
    pub spare: Vec<u8>,
    /// Original current profile, used to identify its key slots.
    pub active: Vec<u8>,
}

/// Commands issued by the calibration wizard, never by the GUI's HID stack.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum G604Command {
    /// Read state or a pending crash recovery record.
    Status,
    /// Back up and begin calibration of G4 through G9.
    Begin,
    /// Start device-attributed capture for the next physical button.
    Calibrate(u8),
    /// Write the dedicated trigger profile after all six slots are known.
    Enable,
    /// Begin physical F13–F18 validation of one side key.
    Verify(u8),
    /// Commit successful physical validation and enable runtime interception.
    Accept,
    /// Restore sectors, mode, profile and DPI from the durable journal.
    Restore,
}

/// Cached calibration state; no hardware reads during status polling.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct G604Status {
    /// Side-button index currently awaiting a physical press.
    pub awaiting: Option<u8>,
    /// Physical G4–G9 mapped to protocol slots.
    pub slots: [Option<u8>; 6],
    /// F13–F18 observed from the actual Bluetooth device.
    pub verified: [bool; 6],
    /// A journal exists and restoration is available.
    pub recovery_pending: bool,
    /// Trigger profile is written; physical verification is still required.
    pub prepared: bool,
    /// All six keys have passed, runtime interception is active.
    pub enabled: bool,
    /// Last hardware or calibration failure.
    pub error: Option<String>,
}
