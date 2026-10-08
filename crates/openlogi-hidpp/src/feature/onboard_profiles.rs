//! Gaming onboard profile access (0x8100).
//!
//! The profile sector layout and memory functions are reverse-engineered,
//! independently implemented from Solaar's `OnboardProfiles` observations:
//! <https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp20.py>.
//! Unknown formats are rejected rather than rewritten.

use crate::{feature::FeatureEndpoint, protocol::v20::Hidpp20Error};
use num_enum::TryFromPrimitive;
use openlogi_hidpp_derive::Feature;

/// Firmware execution owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum ProfileMode {
    /// Mouse firmware executes the selected onboard mapping.
    Onboard = 1,
    /// Host software manages the device.
    Host = 2,
}

/// Memory geometry reported by the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardDescriptor {
    /// Number of writable profiles.
    pub profiles: u8,
    /// Number of button slots per profile.
    pub buttons: u8,
    /// Number of writable sectors, including the directory sector.
    pub sectors: u8,
    /// Exact sector length, including its final two CRC bytes.
    pub sector_size: u16,
}

impl OnboardDescriptor {
    /// Accept only the known memory/profile format used by this G604.
    pub fn decode(payload: [u8; 16]) -> Result<Self, Hidpp20Error> {
        let sector_size = u16::from_be_bytes([payload[7], payload[8]]);
        if payload[0] != 1
            || payload[1] != 3
            || payload[5] != 16
            || payload[3] < 2
            || payload[6] < 3
            || !(255..=512).contains(&sector_size)
        {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        Ok(Self {
            profiles: payload[3],
            buttons: payload[5],
            sectors: payload[6],
            sector_size,
        })
    }
}

/// Implements the subset needed for a backed-up, reversible trigger profile.
#[derive(Clone, Feature)]
#[creatable(id = 0x8100, version = 0)]
pub struct OnboardProfilesFeature {
    endpoint: FeatureEndpoint,
}

impl OnboardProfilesFeature {
    /// Original descriptor bytes, retained in the recovery journal.
    pub async fn raw_descriptor(&self) -> Result<[u8; 16], Hidpp20Error> {
        Ok(self.endpoint.call(0, [0; 3]).await?.extend_payload())
    }
    /// Read and validate memory geometry.
    pub async fn descriptor(&self) -> Result<OnboardDescriptor, Hidpp20Error> {
        OnboardDescriptor::decode(self.raw_descriptor().await?)
    }
    /// Read the current host/onboard mode.
    pub async fn mode(&self) -> Result<ProfileMode, Hidpp20Error> {
        ProfileMode::try_from(self.endpoint.call(2, [0; 3]).await?.extend_payload()[0])
            .map_err(|_| Hidpp20Error::UnsupportedResponse)
    }
    /// Select an execution mode, then verify its read-back.
    pub async fn select_mode(&self, mode: ProfileMode) -> Result<(), Hidpp20Error> {
        self.endpoint
            .call_long_write_through(1, payload(&[mode as u8]))
            .await?;
        if self.mode().await? != mode {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        Ok(())
    }
    /// Read the active profile sector.
    pub async fn profile(&self) -> Result<u16, Hidpp20Error> {
        let p = self.endpoint.call(4, [0; 3]).await?.extend_payload();
        Ok(u16::from_be_bytes([p[0], p[1]]))
    }
    /// Select an onboard profile and verify it.
    pub async fn select_profile(&self, sector: u16) -> Result<(), Hidpp20Error> {
        self.endpoint
            .call_long_write_through(3, payload(&sector.to_be_bytes()))
            .await?;
        if self.profile().await? != sector {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        Ok(())
    }
    /// Read the exact sector size, overlapping the final 16-byte request.
    pub async fn read_sector(&self, sector: u16, size: u16) -> Result<Vec<u8>, Hidpp20Error> {
        if !(16..=512).contains(&size) {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        let mut result = Vec::with_capacity(usize::from(size));
        let mut offset = 0u16;
        while offset + 16 <= size {
            let chunk = self.read_chunk(sector, offset).await?;
            result.extend_from_slice(&chunk);
            offset += 16;
        }
        if offset < size {
            let chunk = self.read_chunk(sector, size - 16).await?;
            result.extend_from_slice(&chunk[usize::from(offset - (size - 16))..]);
        }
        Ok(result)
    }
    async fn read_chunk(&self, sector: u16, offset: u16) -> Result<[u8; 16], Hidpp20Error> {
        let [a, b] = sector.to_be_bytes();
        let [c, d] = offset.to_be_bytes();
        Ok(self
            .endpoint
            .call_long(5, payload(&[a, b, c, d]))
            .await?
            .extend_payload())
    }
    /// Write exactly one CRC-valid sector and compare every byte after commit.
    /// The caller must preserve an original backup before entering this operation.
    pub async fn write_sector(&self, sector: u16, bytes: &[u8]) -> Result<(), Hidpp20Error> {
        if !(16..=512).contains(&bytes.len()) || !valid_crc(bytes) {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        let size = u16::try_from(bytes.len()).map_err(|_| Hidpp20Error::UnsupportedResponse)?;
        let [a, b] = sector.to_be_bytes();
        let [c, d] = size.to_be_bytes();
        self.endpoint
            .call_long_write_through(6, payload(&[a, b, 0, 0, c, d]))
            .await?;
        for chunk in bytes.chunks(16) {
            self.endpoint
                .call_long_write_through(7, payload(chunk))
                .await?;
        }
        self.endpoint.call_long_write_through(8, [0; 16]).await?;
        if self.read_sector(sector, size).await? != bytes {
            return Err(Hidpp20Error::UnsupportedResponse);
        }
        Ok(())
    }
}

fn payload(bytes: &[u8]) -> [u8; 16] {
    let mut payload = [0; 16];
    payload[..bytes.len()].copy_from_slice(bytes);
    payload
}

/// CRC-16/CCITT-FALSE, stored big-endian in the final two sector bytes.
#[must_use]
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Whether a complete sector has its expected trailing CRC.
#[must_use]
pub fn valid_crc(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && crc16(&bytes[..bytes.len() - 2]).to_be_bytes() == bytes[bytes.len() - 2..]
}

/// Recompute the CRC after a local edit without rebuilding unknown fields.
pub fn update_crc(bytes: &mut [u8]) -> Result<(), Hidpp20Error> {
    let len = bytes.len();
    if len < 2 {
        return Err(Hidpp20Error::UnsupportedResponse);
    }
    let crc = crc16(&bytes[..len - 2]).to_be_bytes();
    bytes[len - 2..].copy_from_slice(&crc);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn g604_geometry_and_crc_match_independent_values() {
        let descriptor =
            OnboardDescriptor::decode([1, 3, 1, 5, 1, 16, 16, 0, 255, 10, 4, 0, 0, 0, 0, 0])
                .unwrap();
        assert_eq!(descriptor.sector_size, 255);
        assert_eq!(crc16(b"123456789"), 0x29b1);
        let mut sector = vec![0xff; 255];
        update_crc(&mut sector).unwrap();
        assert_eq!(sector.len(), 255);
        assert!(valid_crc(&sector));
        sector[17] ^= 1;
        assert!(!valid_crc(&sector));
    }
    #[test]
    fn unknown_geometry_is_rejected() {
        OnboardDescriptor::decode([0; 16]).unwrap_err();
    }
}
