use hidpp::protocol::v20::{ErrorType, Hidpp20Error};

// WriteError, HidppOperation, and HidppFeatureErrorKind are pure IPC wire
// data with no HID++/backend I/O, so they live in
// `openlogi_core::hid::error`; the write path names them through here. The
// conversions below stay here because they name `hidpp` and backend types,
// which `openlogi-core` must never depend on.
pub use openlogi_core::hid::{HidppFeatureErrorKind, HidppOperation, WriteError};

fn hidpp_feature_error_kind(kind: ErrorType) -> HidppFeatureErrorKind {
    match kind {
        ErrorType::NoError => HidppFeatureErrorKind::NoError,
        ErrorType::Unknown => HidppFeatureErrorKind::Unknown,
        ErrorType::InvalidArgument => HidppFeatureErrorKind::InvalidArgument,
        ErrorType::OutOfRange => HidppFeatureErrorKind::OutOfRange,
        ErrorType::HwError => HidppFeatureErrorKind::HwError,
        ErrorType::LogitechInternal => HidppFeatureErrorKind::LogitechInternal,
        ErrorType::InvalidFeatureIndex => HidppFeatureErrorKind::InvalidFeatureIndex,
        ErrorType::InvalidFunctionId => HidppFeatureErrorKind::InvalidFunctionId,
        ErrorType::Busy => HidppFeatureErrorKind::Busy,
        ErrorType::Unsupported => HidppFeatureErrorKind::Unsupported,
        _ => HidppFeatureErrorKind::Unrecognized,
    }
}

pub(crate) fn classify_hidpp_error(
    error: Hidpp20Error,
    operation: HidppOperation,
    feature_hex: u16,
) -> WriteError {
    match error {
        Hidpp20Error::Feature(kind) => WriteError::HidppFeature {
            operation,
            feature_hex,
            kind: hidpp_feature_error_kind(kind),
        },
        Hidpp20Error::UnsupportedResponse => WriteError::UnsupportedResponse {
            operation,
            feature_hex,
        },
        Hidpp20Error::Channel(hidpp::channel::ChannelError::Timeout) => {
            WriteError::RequestTimedOut { operation }
        }
        Hidpp20Error::Channel(error) => WriteError::Hidpp(format!("{error:?}")),
        _ => WriteError::Hidpp(format!("{error:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hidpp::channel::ChannelError;

    #[test]
    fn channel_timeout_preserves_the_operation_for_read_retry() {
        assert_eq!(
            classify_hidpp_error(
                Hidpp20Error::Channel(ChannelError::Timeout),
                HidppOperation::WriteOnboardProfiles,
                0x8100,
            ),
            WriteError::RequestTimedOut {
                operation: HidppOperation::WriteOnboardProfiles,
            }
        );
        assert_eq!(
            classify_hidpp_error(
                Hidpp20Error::Feature(ErrorType::Unsupported),
                HidppOperation::WriteOnboardProfiles,
                0x8100,
            ),
            WriteError::HidppFeature {
                operation: HidppOperation::WriteOnboardProfiles,
                feature_hex: 0x8100,
                kind: HidppFeatureErrorKind::Unsupported,
            }
        );
    }
}
