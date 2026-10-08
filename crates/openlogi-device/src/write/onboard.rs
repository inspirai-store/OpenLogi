//! Reversible, format-checked G604 profile transactions over the agent's channel.
use super::{classify_hidpp_error, open_feature};
use crate::{DeviceRoute, Dpi, HidppOperation, SharedChannel, WriteError};
use hidpp::device::{Device, DeviceError};
use hidpp::feature::onboard_profiles::{
    OnboardDescriptor, OnboardProfilesFeature, ProfileMode, update_crc, valid_crc,
};
use openlogi_core::macros::g604::OnboardBackup;

async fn feature(
    channel: &SharedChannel,
) -> Result<std::sync::Arc<OnboardProfilesFeature>, WriteError> {
    if !matches!(
        channel.route(),
        DeviceRoute::Direct {
            vendor_id: 0x046d,
            product_id: 0xb024
        }
    ) {
        return Err(WriteError::FeatureUnsupported {
            feature_hex: 0x8100,
        });
    }
    read_query("connect and resolve 0x8100", || async {
        let mut device = Device::new(
            std::sync::Arc::clone(channel.channel()),
            channel.device_index(),
        )
        .await
        .map_err(|e| match e {
            DeviceError::Channel(error) => self::error(error.into()),
            other => WriteError::Hidpp(other.to_string()),
        })?;
        open_feature::<OnboardProfilesFeature>(&mut device).await
    })
    .await
}

// Only queries use this helper. A lost Bluetooth reply may recover on the
// next request; sector writes must not be replayed after an ambiguous timeout.
async fn read_query<T, F, Fut>(stage: &str, mut read: F) -> Result<T, WriteError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, WriteError>>,
{
    let result = match read().await {
        Err(WriteError::RequestTimedOut { .. }) => {
            tracing::warn!(stage, "G604 read timed out; retrying once");
            read().await
        }
        result => result,
    };
    result.map_err(|error| stage_error(stage, &error))
}

fn stage_error(stage: &str, error: &WriteError) -> WriteError {
    tracing::warn!(stage, %error, "G604 transaction step failed");
    WriteError::Hidpp(format!("G604 {stage}: {error}"))
}
fn error(e: hidpp::protocol::v20::Hidpp20Error) -> WriteError {
    classify_hidpp_error(e, HidppOperation::WriteOnboardProfiles, 0x8100)
}
fn invalid() -> WriteError {
    error(hidpp::protocol::v20::Hidpp20Error::UnsupportedResponse)
}

/// Validate a persisted recovery record before indexing or writing its sectors.
pub fn validate_backup(backup: &OnboardBackup) -> Result<(), WriteError> {
    let descriptor: [u8; 16] = backup
        .descriptor
        .as_slice()
        .try_into()
        .map_err(|_| invalid())?;
    let geometry = OnboardDescriptor::decode(descriptor).map_err(error)?;
    if !matches!(
        backup.route,
        DeviceRoute::Direct {
            vendor_id: 0x046d,
            product_id: 0xb024
        }
    ) || !matches!((backup.mode, backup.profile), (2, 0) | (1, 1..))
        || backup.profile >= u16::from(geometry.sectors)
        || backup.dpi == 0
        || [&backup.directory, &backup.spare, &backup.active]
            .into_iter()
            .any(|sector| sector.len() != usize::from(geometry.sector_size) || !valid_crc(sector))
        || backup.directory[..2] != [0, 1]
        || backup.directory[4..6] != [0, 2]
        || backup.directory[6] != 0
    {
        return Err(invalid());
    }
    Ok(())
}

/// Read all sectors that might be changed, plus the active key map and current DPI.
pub async fn backup_on(channel: &SharedChannel) -> Result<OnboardBackup, WriteError> {
    let f = feature(channel).await?;
    let descriptor = read_query("read descriptor", || async {
        f.raw_descriptor().await.map_err(error)
    })
    .await?;
    let geometry = OnboardDescriptor::decode(descriptor).map_err(error)?;
    let descriptor = descriptor.to_vec();
    let mode = read_query("read mode", || async { f.mode().await.map_err(error) }).await? as u8;
    let profile = read_query("read active profile", || async {
        f.profile().await.map_err(error)
    })
    .await?;
    let directory = read_query("back up directory sector 0", || async {
        f.read_sector(0, geometry.sector_size).await.map_err(error)
    })
    .await?;
    if !valid_crc(&directory)
        || directory[..2] != [0, 1]
        || directory[4..6] != [0, 2]
        || directory[6] != 0
    {
        return Err(invalid());
    }
    let spare = read_query("back up spare sector 2", || async {
        f.read_sector(2, geometry.sector_size).await.map_err(error)
    })
    .await?;
    let active_sector = if profile == 0 { 1 } else { profile };
    if active_sector >= u16::from(geometry.sectors) {
        return Err(invalid());
    }
    let active = read_query("back up active profile", || async {
        f.read_sector(active_sector, geometry.sector_size)
            .await
            .map_err(error)
    })
    .await?;
    if !valid_crc(&spare) || !valid_crc(&active) {
        return Err(invalid());
    }
    let dpi = read_query("read DPI", || super::get_dpi_info_on(channel))
        .await?
        .current
        .into_inner();
    Ok(OnboardBackup {
        route: channel.route().clone(),
        descriptor,
        mode,
        profile,
        dpi,
        directory,
        spare,
        active,
    })
}

/// Select the original profile for device-attributed side-button calibration.
pub async fn calibration_on(
    channel: &SharedChannel,
    backup: &OnboardBackup,
) -> Result<(), WriteError> {
    validate_backup(backup)?;
    let f = feature(channel).await?;
    f.select_mode(ProfileMode::Onboard)
        .await
        .map_err(|e| stage_error("select onboard mode for calibration", &error(e)))?;
    f.select_profile(if backup.profile == 0 {
        1
    } else {
        backup.profile
    })
    .await
    .map_err(|e| stage_error("select calibration profile", &error(e)))?;
    super::set_dpi_on(channel, Dpi::new(backup.dpi))
        .await
        .map_err(|e| stage_error("preserve calibration DPI", &e))
}

/// Patch six calibrated slots and the active DPI preset while preserving all other bytes.
pub fn trigger_sector(backup: &OnboardBackup, slots: [u8; 6]) -> Result<Vec<u8>, WriteError> {
    let mut sector = backup.spare.clone();
    if !(255..=512).contains(&sector.len())
        || !valid_crc(&sector)
        || slots.iter().any(|s| *s >= 16)
        || slots
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != 6
    {
        return Err(invalid());
    }
    for (index, slot) in slots.into_iter().enumerate() {
        let offset = 32 + usize::from(slot) * 4;
        sector[offset..offset + 4].copy_from_slice(&[
            0x80,
            2,
            0,
            0x68 + u8::try_from(index).map_err(|_| invalid())?,
        ]);
    }
    // Reverse-engineered format 3: default DPI index at byte 1; five LE presets at byte 3.
    let dpi_index = usize::from(sector[1]);
    if dpi_index >= 5 {
        return Err(invalid());
    }
    let offset = 3 + dpi_index * 2;
    sector[offset..offset + 2].copy_from_slice(&backup.dpi.to_le_bytes());
    update_crc(&mut sector).map_err(error)?;
    Ok(sector)
}

/// Write and read back the dedicated profile, then select it and retain the original DPI.
pub async fn enable_on(
    channel: &SharedChannel,
    backup: &OnboardBackup,
    slots: [u8; 6],
) -> Result<(), WriteError> {
    validate_backup(backup)?;
    let f = feature(channel).await?;
    // Reject geometry drift and unrelated edits made after the backup.
    if f.raw_descriptor().await.map_err(error)?.as_slice() != backup.descriptor
        || f.read_sector(
            0,
            u16::try_from(backup.directory.len()).map_err(|_| invalid())?,
        )
        .await
        .map_err(error)?
            != backup.directory
        || f.read_sector(2, u16::try_from(backup.spare.len()).map_err(|_| invalid())?)
            .await
            .map_err(error)?
            != backup.spare
    {
        return Err(invalid());
    }
    let sector = trigger_sector(backup, slots)?;
    let mut directory = backup.directory.clone();
    directory[6] = 1;
    update_crc(&mut directory).map_err(error)?;
    f.write_sector(2, &sector)
        .await
        .map_err(|e| stage_error("write and verify trigger sector 2", &error(e)))?;
    f.write_sector(0, &directory)
        .await
        .map_err(|e| stage_error("write and verify directory sector 0", &error(e)))?;
    f.select_mode(ProfileMode::Onboard)
        .await
        .map_err(|e| stage_error("select trigger onboard mode", &error(e)))?;
    f.select_profile(2)
        .await
        .map_err(|e| stage_error("select trigger profile 2", &error(e)))?;
    super::set_dpi_on(channel, Dpi::new(backup.dpi)).await?;
    if super::get_dpi_info_on(channel).await?.current.into_inner() != backup.dpi {
        return Err(invalid());
    }
    Ok(())
}

/// Restore every changed sector and the pre-takeover mode/profile/DPI; verify before returning.
pub async fn restore_on(channel: &SharedChannel, backup: &OnboardBackup) -> Result<(), WriteError> {
    validate_backup(backup)?;
    let f = feature(channel).await?;
    if f.raw_descriptor().await.map_err(error)?.as_slice() != backup.descriptor {
        return Err(invalid());
    }
    let size = u16::try_from(backup.spare.len()).map_err(|_| invalid())?;
    if f.read_sector(2, size).await.map_err(error)? != backup.spare {
        f.write_sector(2, &backup.spare).await.map_err(error)?;
    }
    if f.read_sector(0, size).await.map_err(error)? != backup.directory {
        f.write_sector(0, &backup.directory).await.map_err(error)?;
    }
    if backup.mode == 1 {
        f.select_mode(ProfileMode::Onboard).await.map_err(error)?;
        f.select_profile(backup.profile).await.map_err(error)?;
    } else if backup.mode == 2 {
        f.select_mode(ProfileMode::Host).await.map_err(error)?;
    } else {
        return Err(invalid());
    }
    super::set_dpi_on(channel, Dpi::new(backup.dpi)).await?;
    if f.mode().await.map_err(error)? as u8 != backup.mode
        || f.profile().await.map_err(error)? != backup.profile
        || super::get_dpi_info_on(channel).await?.current.into_inner() != backup.dpi
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lost_read_reply_retries_once_but_permanent_errors_do_not() {
        let attempts = std::cell::Cell::new(0);
        let result = read_query("read mode", || async {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(WriteError::RequestTimedOut {
                    operation: HidppOperation::WriteOnboardProfiles,
                })
            } else {
                Ok(2)
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 2);
        assert_eq!(attempts.get(), 2);

        attempts.set(0);
        let result = read_query::<(), _, _>("read descriptor", || async {
            attempts.set(attempts.get() + 1);
            Err(WriteError::FeatureUnsupported {
                feature_hex: 0x8100,
            })
        })
        .await;
        assert_eq!(attempts.get(), 1);
        assert!(result.unwrap_err().to_string().contains("read descriptor"));
    }

    #[tokio::test]
    async fn repeated_read_timeout_is_bounded_and_names_the_failed_sector() {
        let attempts = std::cell::Cell::new(0);
        let result = read_query::<(), _, _>("back up spare sector 2", || async {
            attempts.set(attempts.get() + 1);
            Err(WriteError::RequestTimedOut {
                operation: HidppOperation::WriteOnboardProfiles,
            })
        })
        .await;
        assert_eq!(attempts.get(), 2);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("spare sector 2"));
        assert!(message.contains("timed out"));
    }
    #[test]
    fn six_slots_and_one_dpi_only_are_patched_in_255_byte_sector() {
        let mut bytes = vec![0xa5; 255];
        bytes[1] = 0;
        update_crc(&mut bytes).unwrap();
        let backup = OnboardBackup {
            route: DeviceRoute::Direct {
                vendor_id: 0x046d,
                product_id: 0xb024,
            },
            descriptor: vec![],
            mode: 2,
            profile: 0,
            dpi: 800,
            directory: bytes.clone(),
            spare: bytes.clone(),
            active: bytes.clone(),
        };
        let slots = [3, 4, 5, 6, 7, 8];
        let result = trigger_sector(&backup, slots).unwrap();
        assert!(valid_crc(&result));
        for i in 0..253 {
            if !(3..5).contains(&i) && !(44..68).contains(&i) {
                assert_eq!(result[i], bytes[i], "byte {i}");
            }
        }
        assert_eq!(&result[44..48], &[0x80, 2, 0, 0x68]);
        trigger_sector(&backup, [3, 3, 5, 6, 7, 8]).unwrap_err();
    }
}
