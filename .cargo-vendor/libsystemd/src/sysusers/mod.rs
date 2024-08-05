//! Helpers for working with `sysusers.d` configuration files.
//!
//! For the complete documentation see
//! <https://www.freedesktop.org/software/systemd/man/sysusers.d.html>.
//!
//! ## Example
//!
//! ```rust
//! # fn doctest_parse() -> Result<(), libsystemd::errors::SdError> {
//! use libsystemd::sysusers;
//!
//! let config_fragment = r#"
//! #Type Name     ID             GECOS                 Home directory Shell
//! u     httpd    404            "HTTP User"
//! u     _authd   /usr/bin/authd "Authorization user"
//! u     postgres -              "Postgresql Database" /var/lib/pgsql /usr/libexec/postgresdb
//! g     input    -              -
//! m     _authd   input
//! u     root     0              "Superuser"           /root          /bin/zsh
//! r     -        500-900
//! "#;
//!
//! let mut reader = config_fragment.as_bytes();
//! let entries = sysusers::parse_from_reader(&mut reader)?;
//! assert_eq!(entries.len(), 7);
//!
//! let users_and_groups: Vec<_> = entries
//!     .into_iter()
//!     .filter_map(|v| {
//!         match v.type_signature() {
//!             "u" | "g" => Some(v.name().to_string()),
//!             _ => None,
//!         }
//!     })
//!     .collect();
//! assert_eq!(users_and_groups, vec!["httpd", "_authd", "postgres", "input", "root"]);
//! # Ok(())
//! # }
//! # doctest_parse().unwrap();
//! ```

pub(crate) use self::serialization::SysusersData;
use crate::errors::{Context, SdError};
pub use parse::parse_from_reader;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::BufRead;
use std::path::PathBuf;
use std::str::FromStr;

mod format;
mod parse;
mod serialization;

/// Single entry in `sysusers.d` configuration format.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SysusersEntry {
    AddRange(AddRange),
    AddUserToGroup(AddUserToGroup),
    CreateGroup(CreateGroup),
    CreateUserAndGroup(CreateUserAndGroup),
}

impl SysusersEntry {
    /// Return the single-character signature for the "Type" field of this entry.
    pub fn type_signature(&self) -> &str {
        match self {
            SysusersEntry::AddRange(v) => v.type_signature(),
            SysusersEntry::AddUserToGroup(v) => v.type_signature(),
            SysusersEntry::CreateGroup(v) => v.type_signature(),
            SysusersEntry::CreateUserAndGroup(v) => v.type_signature(),
        }
    }

    /// Return the value for the "Name" field of this entry.
    pub fn name(&self) -> &str {
        match self {
            SysusersEntry::AddRange(_) => "-",
            SysusersEntry::AddUserToGroup(v) => &v.username,
            SysusersEntry::CreateGroup(v) => &v.groupname,
            SysusersEntry::CreateUserAndGroup(v) => &v.name,
        }
    }
}

/// Sysusers entry of type `r`.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "SysusersData")]
pub struct AddRange {
    pub(crate) from: u32,
    pub(crate) to: u32,
}

impl AddRange {
    /// Create a new `AddRange` entry.
    pub fn new(from: u32, to: u32) -> Result<Self, SdError> {
        Ok(Self { from, to })
    }

    /// Return the single-character signature for the "Type" field of this entry.
    pub fn type_signature(&self) -> &str {
        "r"
    }

    /// Return the lower end for the range of this entry.
    pub fn from(&self) -> u32 {
        self.from
    }

    /// Return the upper end for the range of this entry.
    pub fn to(&self) -> u32 {
        self.to
    }

    pub(crate) fn into_sysusers_entry(self) -> SysusersEntry {
        SysusersEntry::AddRange(self)
    }
}

/// Sysusers entry of type `m`.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(try_from = "SysusersData")]
pub struct AddUserToGroup {
    pub(crate) username: String,
    pub(crate) groupname: String,
}

impl AddUserToGroup {
    /// Create a new `AddUserToGroup` entry.
    pub fn new(username: String, groupname: String) -> Result<Self, SdError> {
        validate_name_strict(&username)?;
        validate_name_strict(&groupname)?;
        Ok(Self {
            username,
            groupname,
        })
    }

    /// Return the single-character signature for the "Type" field of this entry.
    pub fn type_signature(&self) -> &str {
        "m"
    }

    /// Return the user name ("Name" field) of this entry.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Return the group name ("ID" field) of this entry.
    pub fn groupname(&self) -> &str {
        &self.groupname
    }

    pub(crate) fn into_sysusers_entry(self) -> SysusersEntry {
        SysusersEntry::AddUserToGroup(self)
    }
}

/// Sysusers entry of type `g`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "SysusersData")]
pub struct CreateGroup {
    pub(crate) groupname: String,
    pub(crate) gid: GidOrPath,
}

impl CreateGroup {
    /// Create a new `CreateGroup` entry.
    pub fn new(groupname: String) -> Result<Self, SdError> {
        Self::impl_new(groupname, GidOrPath::Automatic)
    }

    /// Create a new `CreateGroup` entry, using a numeric ID.
    pub fn new_with_gid(groupname: String, gid: u32) -> Result<Self, SdError> {
        Self::impl_new(groupname, GidOrPath::Gid(gid))
    }

    /// Create a new `CreateGroup` entry, using a filepath reference.
    pub fn new_with_path(groupname: String, path: PathBuf) -> Result<Self, SdError> {
        Self::impl_new(groupname, GidOrPath::Path(path))
    }

    pub(crate) fn impl_new(groupname: String, gid: GidOrPath) -> Result<Self, SdError> {
        validate_name_strict(&groupname)?;
        Ok(Self { groupname, gid })
    }

    /// Return the single-character signature for the "Type" field of this entry.
    pub fn type_signature(&self) -> &str {
        "g"
    }

    /// Return the group name ("Name" field) of this entry.
    pub fn groupname(&self) -> &str {
        &self.groupname
    }

    /// Return whether GID is dynamically allocated at runtime.
    pub fn has_dynamic_gid(&self) -> bool {
        matches!(self.gid, GidOrPath::Automatic)
    }

    /// Return the group identifier (GID) of this entry, if statically set.
    pub fn static_gid(&self) -> Option<u32> {
        match self.gid {
            GidOrPath::Gid(n) => Some(n),
            _ => None,
        }
    }

    pub(crate) fn into_sysusers_entry(self) -> SysusersEntry {
        SysusersEntry::CreateGroup(self)
    }
}

/// Sysusers entry of type `u`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "SysusersData")]
pub struct CreateUserAndGroup {
    pub(crate) name: String,
    pub(crate) id: IdOrPath,
    pub(crate) gecos: String,
    pub(crate) home_dir: Option<PathBuf>,
    pub(crate) shell: Option<PathBuf>,
}

impl CreateUserAndGroup {
    /// Create a new `CreateUserAndGroup` entry, using a filepath reference.
    pub fn new(
        name: String,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
    ) -> Result<Self, SdError> {
        Self::impl_new(name, gecos, home_dir, shell, IdOrPath::Automatic)
    }

    /// Create a new `CreateUserAndrGroup` entry, using a numeric ID.
    pub fn new_with_id(
        name: String,
        id: u32,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
    ) -> Result<Self, SdError> {
        Self::impl_new(name, gecos, home_dir, shell, IdOrPath::Id(id))
    }

    /// Create a new `CreateUserAndGroup` entry, using a UID and a GID.
    pub fn new_with_uid_gid(
        name: String,
        uid: u32,
        gid: u32,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
    ) -> Result<Self, SdError> {
        Self::impl_new(name, gecos, home_dir, shell, IdOrPath::UidGid((uid, gid)))
    }

    /// Create a new `CreateUserAndGroup` entry, using a UID and a groupname.
    pub fn new_with_uid_groupname(
        name: String,
        uid: u32,
        groupname: String,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
    ) -> Result<Self, SdError> {
        validate_name_strict(&groupname)?;
        Self::impl_new(
            name,
            gecos,
            home_dir,
            shell,
            IdOrPath::UidGroupname((uid, groupname)),
        )
    }

    /// Create a new `CreateUserAndGroup` entry, using a filepath reference.
    pub fn new_with_path(
        name: String,
        path: PathBuf,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
    ) -> Result<Self, SdError> {
        Self::impl_new(name, gecos, home_dir, shell, IdOrPath::Path(path))
    }

    pub(crate) fn impl_new(
        name: String,
        gecos: String,
        home_dir: Option<PathBuf>,
        shell: Option<PathBuf>,
        id: IdOrPath,
    ) -> Result<Self, SdError> {
        validate_name_strict(&name)?;
        Ok(Self {
            name,
            id,
            gecos,
            home_dir,
            shell,
        })
    }

    /// Return the single-character signature for the "Type" field of this entry.
    pub fn type_signature(&self) -> &str {
        "u"
    }

    /// Return the user and group name ("Name" field) of this entry.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return whether UID and GID are dynamically allocated at runtime.
    pub fn has_dynamic_ids(&self) -> bool {
        matches!(self.id, IdOrPath::Automatic)
    }

    /// Return the user identifier (UID) of this entry, if statically set.
    pub fn static_uid(&self) -> Option<u32> {
        match self.id {
            IdOrPath::Id(n) => Some(n),
            IdOrPath::UidGid((n, _)) => Some(n),
            IdOrPath::UidGroupname((n, _)) => Some(n),
            _ => None,
        }
    }

    /// Return the groups identifier (GID) of this entry, if statically set.
    pub fn static_gid(&self) -> Option<u32> {
        match self.id {
            IdOrPath::Id(n) => Some(n),
            IdOrPath::UidGid((_, n)) => Some(n),
            _ => None,
        }
    }

    pub(crate) fn into_sysusers_entry(self) -> SysusersEntry {
        SysusersEntry::CreateUserAndGroup(self)
    }
}

/// ID entity for `CreateUserAndGroup`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum IdOrPath {
    Id(u32),
    UidGid((u32, u32)),
    UidGroupname((u32, String)),
    Path(PathBuf),
    Automatic,
}

impl FromStr for IdOrPath {
    type Err = SdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "-" {
            return Ok(IdOrPath::Automatic);
        }
        if value.starts_with('/') {
            return Ok(IdOrPath::Path(value.into()));
        }
        if let Ok(single_id) = value.parse() {
            return Ok(IdOrPath::Id(single_id));
        }
        let tokens: Vec<_> = value.split(':').filter(|s| !s.is_empty()).collect();
        if tokens.len() == 2 {
            let uid: u32 = tokens[0].parse().context("invalid user id")?;
            let id = match tokens[1].parse() {
                Ok(gid) => IdOrPath::UidGid((uid, gid)),
                _ => {
                    let groupname = tokens[1].to_string();
                    validate_name_strict(&groupname).context("name failed validation")?;
                    IdOrPath::UidGroupname((uid, groupname))
                }
            };
            return Ok(id);
        }

        Err(format!("unexpected user ID '{}'", value).into())
    }
}

/// ID entity for `CreateGroup`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GidOrPath {
    Gid(u32),
    Path(PathBuf),
    Automatic,
}

impl FromStr for GidOrPath {
    type Err = SdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "-" {
            return Ok(GidOrPath::Automatic);
        }
        if value.starts_with('/') {
            return Ok(GidOrPath::Path(value.into()));
        }
        if let Ok(parsed_gid) = value.parse() {
            return Ok(GidOrPath::Gid(parsed_gid));
        }

        Err(format!("unexpected group ID '{}'", value).into())
    }
}

/// Validate a sysusers name in strict mode.
pub fn validate_name_strict(input: &str) -> Result<(), SdError> {
    if input.is_empty() {
        return Err(SdError::from("empty name"));
    }

    if input.len() > 31 {
        let err_msg = format!(
            "overlong sysusers name '{}' (more than 31 characters)",
            input
        );
        return Err(SdError::from(err_msg));
    }

    for (index, ch) in input.char_indices() {
        if index == 0 {
            if !(ch.is_ascii_alphabetic() || ch == '_') {
                let err_msg = format!(
                    "invalid starting character '{}' in sysusers name '{}'",
                    ch, input
                );
                return Err(SdError::from(err_msg));
            }
        } else if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            let err_msg = format!("invalid character '{}' in sysusers name '{}'", ch, input);
            return Err(SdError::from(err_msg));
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_validate_name_strict() {
        let err_cases = vec!["-foo", "10bar", "42"];
        for entry in err_cases {
            validate_name_strict(entry).unwrap_err();
        }

        let ok_cases = vec!["_authd", "httpd"];
        for entry in ok_cases {
            validate_name_strict(entry).unwrap();
        }
    }
}
