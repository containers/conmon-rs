use crate::errors::{Context, SdError};
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::hash::Hash;
use std::io::Read;
use std::{fmt, fs};
use uuid::{Bytes, Uuid};

/// A 128-bits ID.
#[derive(Clone, Copy, Hash, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Id128 {
    #[serde(flatten, serialize_with = "Id128::ser_uuid")]
    uuid_v4: Uuid,
}

impl Id128 {
    /// Build an `Id128` from a slice of bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, SdError> {
        let uuid_v4 = Uuid::from_slice(bytes).context("failed to parse ID from bytes slice")?;

        // TODO(lucab): check for v4.
        Ok(Self { uuid_v4 })
    }

    /// Build an `Id128` from 16 bytes
    pub const fn from_bytes(bytes: Bytes) -> Self {
        Self {
            uuid_v4: Uuid::from_bytes(bytes),
        }
    }

    /// Parse an `Id128` from string.
    pub fn parse_str<S>(input: S) -> Result<Self, SdError>
    where
        S: AsRef<str>,
    {
        let uuid_v4 = Uuid::parse_str(input.as_ref()).context("failed to parse ID from string")?;

        // TODO(lucab): check for v4.
        Ok(Self { uuid_v4 })
    }

    /// Hash this ID with an application-specific ID.
    pub fn app_specific(&self, app: &Self) -> Result<Self, SdError> {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let mut mac = Hmac::<Sha256>::new_from_slice(self.uuid_v4.as_bytes())
            .map_err(|_| "failed to prepare HMAC")?;
        mac.update(app.uuid_v4.as_bytes());
        let mut hashed = mac.finalize().into_bytes();

        if hashed.len() != 32 {
            return Err("short hash".into());
        };

        // Set version to 4.
        hashed[6] = (hashed[6] & 0x0F) | 0x40;
        // Set variant to DCE.
        hashed[8] = (hashed[8] & 0x3F) | 0x80;

        Self::try_from_slice(&hashed[..16])
    }

    /// Return this ID as a lowercase hexadecimal string, without dashes.
    pub fn lower_hex(&self) -> String {
        let mut hex = String::new();
        for byte in self.uuid_v4.as_bytes() {
            write!(hex, "{byte:02x}").unwrap();
        }
        hex
    }

    /// Return this ID as a lowercase hexadecimal string, with dashes.
    pub fn dashed_hex(&self) -> String {
        format!("{}", self.uuid_v4.hyphenated())
    }

    /// Custom serialization (lower hex).
    fn ser_uuid<S>(field: &Uuid, s: S) -> ::std::result::Result<S::Ok, S::Error>
    where
        S: ::serde::Serializer,
    {
        let mut hex = String::new();
        for byte in field.as_bytes() {
            write!(hex, "{byte:02x}").unwrap();
        }
        s.serialize_str(&hex)
    }
}

impl fmt::Debug for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.dashed_hex())
    }
}

impl From<Uuid> for Id128 {
    fn from(uuid_v4: Uuid) -> Self {
        Self { uuid_v4 }
    }
}

/// Return this machine unique ID.
pub fn get_machine() -> Result<Id128, SdError> {
    let mut buf = String::new();
    let mut fd = fs::File::open("/etc/machine-id").context("failed to open machine-id")?;
    fd.read_to_string(&mut buf)
        .context("failed to read machine-id")?;
    Id128::parse_str(buf.trim_end())
}

/// Return this machine unique ID, hashed with an application-specific ID.
pub fn get_machine_app_specific(app_id: &Id128) -> Result<Id128, SdError> {
    let machine_id = get_machine()?;
    machine_id.app_specific(app_id)
}

/// Return the unique ID of this boot.
pub fn get_boot() -> Result<Id128, SdError> {
    let mut buf = String::new();
    let mut fd =
        fs::File::open("/proc/sys/kernel/random/boot_id").context("failed to open boot_id")?;
    fd.read_to_string(&mut buf)
        .context("failed to read boot_id")?;
    Id128::parse_str(buf.trim_end())
}

/// Return the unique ID of this boot, hashed with an application-specific ID.
pub fn get_boot_app_specific(app_id: &Id128) -> Result<Id128, SdError> {
    get_boot()?.app_specific(app_id)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn basic_parse_str() {
        let input = "2e074e9b299c41a59923c51ae16f279b";
        let id = Id128::parse_str(input).unwrap();
        assert_eq!(id.lower_hex(), input);

        Id128::parse_str("").unwrap_err();
    }

    #[test]
    fn basic_keyed_hash() {
        let input = "2e074e9b299c41a59923c51ae16f279b";
        let machine_id = Id128::parse_str(input).unwrap();
        assert_eq!(input, machine_id.lower_hex());

        let key = "033b1b9b264441fcaa173e9e5bf35c5a";
        let app_id = Id128::parse_str(key).unwrap();
        assert_eq!(key, app_id.lower_hex());

        let expected = "4d4a86c9c6644a479560ded5d19a30c5";
        let hashed_id = Id128::parse_str(expected).unwrap();

        let output = machine_id.app_specific(&app_id).unwrap();
        assert_eq!(output, hashed_id);
    }

    #[test]
    fn basic_from_slice() {
        let input_str = "d86a4e9e4dca45c5bcd9846409bfa1ae";
        let input = [
            0xd8, 0x6a, 0x4e, 0x9e, 0x4d, 0xca, 0x45, 0xc5, 0xbc, 0xd9, 0x84, 0x64, 0x09, 0xbf,
            0xa1, 0xae,
        ];
        let id = Id128::try_from_slice(&input).unwrap();
        assert_eq!(input_str, id.lower_hex());

        Id128::try_from_slice(&[]).unwrap_err();
    }

    #[test]
    fn basic_from_bytes() {
        let input_str = "d86a4e9e4dca45c5bcd9846409bfa1ae";
        let input = [
            0xd8, 0x6a, 0x4e, 0x9e, 0x4d, 0xca, 0x45, 0xc5, 0xbc, 0xd9, 0x84, 0x64, 0x09, 0xbf,
            0xa1, 0xae,
        ];
        let id = Id128::from_bytes(input);
        assert_eq!(input_str, id.lower_hex());
    }

    #[test]
    fn basic_debug() {
        let input = "0b37f793-aeb9-4d67-99e1-6e678d86781f";
        let id = Id128::parse_str(input).unwrap();
        assert_eq!(id.dashed_hex(), input);
    }
}
