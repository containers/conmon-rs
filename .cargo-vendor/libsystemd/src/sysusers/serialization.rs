use super::*;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use std::convert::TryFrom;

/// Number of fields in each sysusers entry.
const SYSUSERS_FIELDS: usize = 6;

/// Intermediate format holding raw data for deserialization.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct SysusersData {
    #[serde(rename(deserialize = "Type"))]
    pub(crate) kind: String,
    #[serde(rename(deserialize = "Name"))]
    pub(crate) name: String,
    #[serde(rename(deserialize = "ID"))]
    pub(crate) id: String,
    #[serde(rename(deserialize = "GECOS"))]
    pub(crate) gecos: Option<String>,
    #[serde(rename(deserialize = "Home directory"))]
    pub(crate) home_dir: Option<String>,
    #[serde(rename(deserialize = "Shell"))]
    pub(crate) shell: Option<String>,
}

impl TryFrom<SysusersData> for AddRange {
    type Error = SdError;

    fn try_from(value: SysusersData) -> Result<Self, Self::Error> {
        if value.kind != "r" {
            return Err(format!("unexpected sysuser entry of type '{}'", value.kind).into());
        };
        ensure_field_none_or_automatic("GECOS", &value.gecos)?;
        ensure_field_none_or_automatic("Home directory", &value.home_dir)?;
        ensure_field_none_or_automatic("Shell", &value.shell)?;

        let tokens: Vec<_> = value.id.split('-').collect();
        let (from, to) = match tokens.len() {
            1 => (tokens[0], tokens[0]),
            2 => (tokens[0], tokens[1]),
            _ => return Err(format!("invalid range specifier '{}'", value.id).into()),
        };
        let from_id = from.parse().map_err(|_| "invalid starting range ID")?;
        let to_id = to.parse().map_err(|_| "invalid ending range ID")?;
        Self::new(from_id, to_id)
    }
}

impl TryFrom<SysusersData> for AddUserToGroup {
    type Error = SdError;

    fn try_from(value: SysusersData) -> Result<Self, Self::Error> {
        if value.kind != "m" {
            return Err(format!("unexpected sysuser entry of type '{}'", value.kind).into());
        }
        ensure_field_none_or_automatic("GECOS", &value.gecos)?;
        ensure_field_none_or_automatic("Home directory", &value.home_dir)?;
        ensure_field_none_or_automatic("Shell", &value.shell)?;

        Self::new(value.name, value.id)
    }
}

impl TryFrom<SysusersData> for CreateGroup {
    type Error = SdError;

    fn try_from(value: SysusersData) -> Result<Self, Self::Error> {
        if value.kind != "g" {
            return Err(format!("unexpected sysuser entry of type '{}'", value.kind).into());
        }
        ensure_field_none_or_automatic("GECOS", &value.gecos)?;
        ensure_field_none_or_automatic("Home directory", &value.home_dir)?;
        ensure_field_none_or_automatic("Shell", &value.shell)?;

        let gid: GidOrPath = value.id.parse()?;
        Self::impl_new(value.name, gid)
    }
}

impl TryFrom<SysusersData> for CreateUserAndGroup {
    type Error = SdError;

    fn try_from(value: SysusersData) -> Result<Self, Self::Error> {
        if value.kind != "u" {
            return Err(format!("unexpected sysuser entry of type '{}'", value.kind).into());
        }

        let id: IdOrPath = value.id.parse()?;
        Self::impl_new(
            value.name,
            value.gecos.unwrap_or_default(),
            value.home_dir.map(Into::into),
            value.shell.map(Into::into),
            id,
        )
    }
}

impl Serialize for AddRange {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AddRange", SYSUSERS_FIELDS)?;
        state.serialize_field("Type", self.type_signature())?;
        state.serialize_field("Name", "-")?;
        state.serialize_field("ID", &format!("{}-{}", self.from, self.to))?;
        state.serialize_field("GECOS", &Option::<String>::None)?;
        state.serialize_field("Home directory", &Option::<String>::None)?;
        state.serialize_field("Shell", &Option::<String>::None)?;
        state.end()
    }
}

impl Serialize for AddUserToGroup {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("AddUserToGroup", SYSUSERS_FIELDS)?;
        state.serialize_field("Type", self.type_signature())?;
        state.serialize_field("Name", &self.username)?;
        state.serialize_field("ID", &self.groupname)?;
        state.serialize_field("GECOS", &Option::<String>::None)?;
        state.serialize_field("Home directory", &Option::<String>::None)?;
        state.serialize_field("Shell", &Option::<String>::None)?;
        state.end()
    }
}

impl Serialize for CreateGroup {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CreateGroup", SYSUSERS_FIELDS)?;
        state.serialize_field("Type", self.type_signature())?;
        state.serialize_field("Name", &self.groupname)?;
        state.serialize_field("ID", &self.gid)?;
        state.serialize_field("GECOS", &Option::<String>::None)?;
        state.serialize_field("Home directory", &Option::<String>::None)?;
        state.serialize_field("Shell", &Option::<String>::None)?;
        state.end()
    }
}

impl Serialize for CreateUserAndGroup {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("CreateUserAndGroup", SYSUSERS_FIELDS)?;
        state.serialize_field("Type", self.type_signature())?;
        state.serialize_field("Name", &self.name)?;
        state.serialize_field("ID", &self.id)?;
        state.serialize_field("GECOS", &self.gecos)?;
        state.serialize_field("Home directory", &self.home_dir)?;
        state.serialize_field("Shell", &self.shell)?;
        state.end()
    }
}

impl Serialize for IdOrPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl Serialize for GidOrPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Ensure that a field value is either missing or using the default value `-`.
fn ensure_field_none_or_automatic(
    field_name: &str,
    input: &Option<impl AsRef<str>>,
) -> Result<(), SdError> {
    if let Some(val) = input {
        if val.as_ref() != "-" {
            return Err(format!("invalid {} content: '{}'", field_name, val.as_ref()).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_serialization() {
        {
            let input = AddRange::new(10, 20).unwrap();
            let expected = r#"{"Type":"r","Name":"-","ID":"10-20","GECOS":null,"Home directory":null,"Shell":null}"#;

            let output = serde_json::to_string(&input).unwrap();
            assert_eq!(output, expected);
            let entry = SysusersEntry::AddRange(input);
            let output = serde_json::to_string(&entry).unwrap();
            assert_eq!(output, expected);
        }
        {
            let input = AddUserToGroup::new("foo3".to_string(), "bar".to_string()).unwrap();
            let expected = r#"{"Type":"m","Name":"foo3","ID":"bar","GECOS":null,"Home directory":null,"Shell":null}"#;

            let output = serde_json::to_string(&input).unwrap();
            assert_eq!(output, expected);
            let entry = SysusersEntry::AddUserToGroup(input);
            let output = serde_json::to_string(&entry).unwrap();
            assert_eq!(output, expected);
        }
        {
            let input = CreateGroup::new("foo1".to_string()).unwrap();
            let expected = r#"{"Type":"g","Name":"foo1","ID":"-","GECOS":null,"Home directory":null,"Shell":null}"#;

            let output = serde_json::to_string(&input).unwrap();
            assert_eq!(output, expected);
            let entry = SysusersEntry::CreateGroup(input);
            let output = serde_json::to_string(&entry).unwrap();
            assert_eq!(output, expected);
        }
        {
            let input = CreateUserAndGroup::new("foo0".to_string(), "test".to_string(), None, None)
                .unwrap();
            let expected = r#"{"Type":"u","Name":"foo0","ID":"-","GECOS":"test","Home directory":null,"Shell":null}"#;

            let output = serde_json::to_string(&input).unwrap();
            assert_eq!(output, expected);
            let entry = SysusersEntry::CreateUserAndGroup(input);
            let output = serde_json::to_string(&entry).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_deserialization() {
        {
            let input = r#"{"Type":"r","Name":"-","ID":"50-200","GECOS":null,"Home directory":null,"Shell":null}"#;
            let expected = AddRange::new(50, 200).unwrap();

            let output: AddRange = serde_json::from_str(input).unwrap();
            assert_eq!(output, expected);
            let output: SysusersEntry = serde_json::from_str(input).unwrap();
            assert_eq!(output, SysusersEntry::AddRange(expected));
        }
        {
            let input = r#"{"Type":"m","Name":"foo3","ID":"bar","GECOS":null,"Home directory":null,"Shell":null}"#;
            let expected = AddUserToGroup::new("foo3".to_string(), "bar".to_string()).unwrap();

            let output: AddUserToGroup = serde_json::from_str(input).unwrap();
            assert_eq!(output, expected);
            let output: SysusersEntry = serde_json::from_str(input).unwrap();
            assert_eq!(output, SysusersEntry::AddUserToGroup(expected));
        }
        {
            let input = r#"{"Type":"g","Name":"foo1","ID":"-","GECOS":null,"Home directory":null,"Shell":null}"#;
            let expected = CreateGroup::new("foo1".to_string()).unwrap();

            let output: CreateGroup = serde_json::from_str(input).unwrap();
            assert_eq!(output, expected);
            let output: SysusersEntry = serde_json::from_str(input).unwrap();
            assert_eq!(output, SysusersEntry::CreateGroup(expected));
        }
        {
            let input = r#"{"Type":"u","Name":"foo0","ID":"-","GECOS":"test","Home directory":null,"Shell":null}"#;
            let expected =
                CreateUserAndGroup::new("foo0".to_string(), "test".to_string(), None, None)
                    .unwrap();

            let output: CreateUserAndGroup = serde_json::from_str(input).unwrap();
            assert_eq!(output, expected);
            let output: SysusersEntry = serde_json::from_str(input).unwrap();
            assert_eq!(output, SysusersEntry::CreateUserAndGroup(expected));
        }
    }

    #[test]
    fn test_serde_roundtrip() {
        {
            let input = AddRange::new(10, 20).unwrap();

            let json = serde_json::to_string(&input).unwrap();
            let output: AddRange = serde_json::from_str(&json).unwrap();
            assert_eq!(output, input);
            let output: SysusersEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(output, SysusersEntry::AddRange(input));
        }
        {
            let input = AddUserToGroup::new("foo3".to_string(), "bar".to_string()).unwrap();

            let json = serde_json::to_string(&input).unwrap();
            let output: AddUserToGroup = serde_json::from_str(&json).unwrap();
            assert_eq!(output, input);
            let output: SysusersEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(output, SysusersEntry::AddUserToGroup(input));
        }
        {
            let input = CreateGroup::new("foo1".to_string()).unwrap();

            let json = serde_json::to_string(&input).unwrap();
            let output: CreateGroup = serde_json::from_str(&json).unwrap();
            assert_eq!(output, input);
            let output: SysusersEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(output, SysusersEntry::CreateGroup(input));
        }
        {
            let input = CreateUserAndGroup::new("foo0".to_string(), "test".to_string(), None, None)
                .unwrap();

            let json = serde_json::to_string(&input).unwrap();
            let output: CreateUserAndGroup = serde_json::from_str(&json).unwrap();
            assert_eq!(output, input);
            let output: SysusersEntry = serde_json::from_str(&json).unwrap();
            assert_eq!(output, SysusersEntry::CreateUserAndGroup(input));
        }
    }
}
