//! The six bytes this receiver is known by.
//!
//! Used in three places, all of them classic AirPlay: the `_raop._tcp` service *instance name*,
//! the `Apple-Challenge` reply this receiver signs (`rtsp::apple_challenge`), and the speaker id
//! a DACP `setproperty` names (`dacp::machine_number`). shairport-sync keeps the same value as
//! `config.ap1_prefix`, taking it from a network interface's MAC.
//!
//! Derived from the device name instead, which makes it **stable across restarts with nothing to
//! persist and nothing to configure**: `-n` already names this device for Spotify Connect, and a
//! sender that remembers "Living Room" keeps recognising it. Taking it from a MAC would tie the identity
//! to which interface came up first, and storing it in a file would add a path to get wrong.
//!
//! Two receivers sharing a name on one network collide — which is the honest answer, since they
//! are indistinguishable to a sender anyway.

use sha1::{Digest, Sha1};

#[derive(Debug, Clone)]
pub(crate) struct DeviceId {
    bytes: [u8; 6],
}

impl DeviceId {
    /// The first six bytes of the name's SHA-1, with the two bits that matter in an address of
    /// this shape fixed: the multicast bit cleared and the locally-administered bit set, so the
    /// result is a well-formed unicast address that cannot collide with a real vendor's.
    pub(crate) fn from_name(name: &str) -> Self {
        let digest = Sha1::digest(name.as_bytes());
        let mut bytes = [0u8; 6];
        bytes.copy_from_slice(&digest[..6]);
        bytes[0] = (bytes[0] | 0b0000_0010) & 0b1111_1110;
        Self { bytes }
    }

    pub(crate) fn bytes(&self) -> [u8; 6] {
        self.bytes
    }

    /// `aa:bb:cc:dd:ee:ff`. Lowercase, matching shairport-sync's own `deviceid` generation
    /// (`shairport.c`'s `apids` buffer, built with `hexchar[] = "0123456789abcdef"`).
    #[cfg_attr(not(feature = "remote-control"), allow(dead_code))]
    pub(crate) fn colon_separated(&self) -> String {
        self.bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The same id as twelve contiguous uppercase hex digits — the `_raop._tcp` instance-name
    /// prefix (shairport-sync's `mdns.c`, `ap1_service_name`, which its own comment describes as
    /// the device id less the colons).
    pub(crate) fn service_name_prefix(&self) -> String {
        self.bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of deriving it: the same name is the same device, restart after restart, with
    /// nothing stored anywhere.
    #[test]
    fn a_name_always_yields_the_same_id() {
        assert_eq!(
            DeviceId::from_name("Test Speaker").bytes(),
            DeviceId::from_name("Test Speaker").bytes()
        );
        assert_ne!(
            DeviceId::from_name("Test Speaker").bytes(),
            DeviceId::from_name("Kitchen").bytes()
        );
    }

    /// A synthesized address has to be well-formed: unicast, and marked as locally administered
    /// so it cannot be mistaken for a real vendor's.
    #[test]
    fn the_id_is_a_locally_administered_unicast_address() {
        let first = DeviceId::from_name("Test Speaker").bytes()[0];

        assert_eq!(first & 0b0000_0001, 0, "not multicast");
        assert_eq!(first & 0b0000_0010, 0b0000_0010, "locally administered");
    }

    #[test]
    fn the_two_spellings_describe_one_id() {
        let id = DeviceId::from_name("Test Speaker");
        let bytes = id.bytes();

        assert_eq!(
            id.colon_separated(),
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        );
        assert_eq!(id.service_name_prefix().len(), 12);
        assert_eq!(
            id.service_name_prefix(),
            id.service_name_prefix().to_uppercase()
        );
    }
}
