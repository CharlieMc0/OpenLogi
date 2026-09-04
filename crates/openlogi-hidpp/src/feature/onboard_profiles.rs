//! Implements `OnboardProfiles` (feature `0x8100`).
//!
//! Read-only diagnostic surface only: OpenLogi does not manage on-device
//! profile memory (see `openlogi diag mouse-buttons`). The layout below is
//! reverse-engineered, cross-checked against two independent sources —
//! libratbag (`src/hidpp20.c` / `hidpp20.h`) and `cvuchener/hidpp`
//! (`hidpp20/IOnboardProfiles.h`) — which agree on the function IDs and the
//! descriptor layout. No official Logitech spec document was found for this
//! feature.

use num_enum::TryFromPrimitive;
use openlogi_hidpp_derive::Feature;

use crate::{feature::FeatureEndpoint, protocol::v20::Hidpp20Error};

/// The device's onboard/host mode, as returned by `getMode`.
///
/// `0` ("no change") is a write-only sentinel on `setMode` and never a state
/// a device reports here — an unrecognised byte, including `0`, is
/// [`Hidpp20Error::UnsupportedResponse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, TryFromPrimitive)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[repr(u8)]
pub enum OnboardMode {
    /// The device applies its onboard profile (DPI stages, button bindings,
    /// macros, RGB) without host involvement.
    Onboard = 1,
    /// The device sends only generic button/DPI events; onboard-profile
    /// bindings do not apply. `0x8110`'s `MouseButtonSpy` button mapping
    /// (functions 3/4, not implemented here) only takes effect in this mode.
    Host = 2,
}

/// The `0x8100` descriptor returned by `getDescription` — memory layout and
/// per-device profile geometry, not a profile's contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub struct ProfilesDescription {
    /// Vendor memory-model identifier.
    pub memory_model_id: u8,
    /// Onboard profile blob format version — load-bearing for any future
    /// profile-memory work: the 256-byte layout other tools reverse-engineered
    /// is specific to one `profile_format_id` value per device family.
    pub profile_format_id: u8,
    /// Macro blob format version.
    pub macro_format_id: u8,
    /// Number of user-writable onboard profiles.
    pub profile_count: u8,
    /// Number of out-of-box (read-only/factory) profiles.
    pub profile_count_oob: u8,
    /// Number of physical buttons the profile format has slots for.
    pub button_count: u8,
    /// Number of addressable memory sectors.
    pub sector_count: u8,
    /// Bytes per memory sector.
    pub sector_size: u16,
    /// Vendor mechanical-layout code.
    pub mechanical_layout: u8,
    /// Vendor-defined additional info byte.
    pub various_info: u8,
}

impl ProfilesDescription {
    fn from_payload(payload: [u8; 16]) -> Self {
        Self {
            memory_model_id: payload[0],
            profile_format_id: payload[1],
            macro_format_id: payload[2],
            profile_count: payload[3],
            profile_count_oob: payload[4],
            button_count: payload[5],
            sector_count: payload[6],
            sector_size: u16::from_be_bytes([payload[7], payload[8]]),
            mechanical_layout: payload[9],
            various_info: payload[10],
        }
    }
}

/// Implements the `OnboardProfiles` / `0x8100` feature.
///
/// Only the read-only diagnostic functions are implemented (`getDescription`,
/// `getMode`). Mode switching and the sector-addressed memory read/write
/// functions are deliberately not implemented — OpenLogi does not manage
/// on-device profile memory.
#[derive(Feature)]
#[creatable(id = 0x8100, version = 0)]
pub struct OnboardProfilesFeature {
    endpoint: FeatureEndpoint,
}

impl OnboardProfilesFeature {
    /// Reads the profile-memory descriptor (`getDescription`, function 0).
    pub async fn get_description(&self) -> Result<ProfilesDescription, Hidpp20Error> {
        let payload = self.endpoint.call(0, [0; 3]).await?.extend_payload();
        Ok(ProfilesDescription::from_payload(payload))
    }

    /// Reads the device's current onboard/host mode (`getMode`, function 2).
    pub async fn get_mode(&self) -> Result<OnboardMode, Hidpp20Error> {
        let byte = self.endpoint.call(2, [0; 3]).await?.extend_payload()[0];
        parse_mode(byte)
    }
}

fn parse_mode(byte: u8) -> Result<OnboardMode, Hidpp20Error> {
    OnboardMode::try_from(byte).map_err(|_| Hidpp20Error::UnsupportedResponse)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::{OnboardMode, ProfilesDescription, parse_mode};
    use crate::protocol::v20::Hidpp20Error;

    #[test]
    fn parses_profiles_description() {
        let mut payload = [0u8; 16];
        payload[0] = 0x01; // memory_model_id
        payload[1] = 0x02; // profile_format_id
        payload[2] = 0x03; // macro_format_id
        payload[3] = 5; // profile_count
        payload[4] = 1; // profile_count_oob
        payload[5] = 16; // button_count
        payload[6] = 32; // sector_count
        payload[7] = 0x01; // sector_size hi
        payload[8] = 0x00; // sector_size lo
        payload[9] = 0x07; // mechanical_layout
        payload[10] = 0x09; // various_info
        payload[11] = 0xff; // trailing bytes, ignored
        payload[15] = 0xff;

        let description = ProfilesDescription::from_payload(payload);

        assert_eq!(description.memory_model_id, 0x01);
        assert_eq!(description.profile_format_id, 0x02);
        assert_eq!(description.macro_format_id, 0x03);
        assert_eq!(description.profile_count, 5);
        assert_eq!(description.profile_count_oob, 1);
        assert_eq!(description.button_count, 16);
        assert_eq!(description.sector_count, 32);
        assert_eq!(description.sector_size, 256);
        assert_eq!(description.mechanical_layout, 0x07);
        assert_eq!(description.various_info, 0x09);
    }

    #[test]
    fn rejects_the_write_only_no_change_sentinel_in_a_response() {
        assert_matches!(parse_mode(0), Err(Hidpp20Error::UnsupportedResponse));
    }

    #[test]
    fn parses_onboard_mode() {
        assert_matches!(parse_mode(1), Ok(OnboardMode::Onboard));
    }

    #[test]
    fn parses_host_mode() {
        assert_matches!(parse_mode(2), Ok(OnboardMode::Host));
    }

    #[test]
    fn rejects_unknown_mode_value() {
        assert_matches!(parse_mode(3), Err(Hidpp20Error::UnsupportedResponse));
        assert_matches!(parse_mode(0xff), Err(Hidpp20Error::UnsupportedResponse));
    }
}
