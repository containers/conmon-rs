use super::*;
use nom::bytes::complete::{tag, take_while1};
use nom::character::complete::{anychar, multispace0, multispace1};
use nom::{Finish, IResult};
use std::convert::TryInto;
use std::str::FromStr;

/// Parse `sysusers.d` configuration entries from a buffered reader.
pub fn parse_from_reader(bufrd: &mut impl BufRead) -> Result<Vec<SysusersEntry>, SdError> {
    use crate::errors::ErrorKind;

    let mut output = vec![];
    for (index, item) in bufrd.lines().enumerate() {
        let linenumber = index.saturating_add(1);
        let line = item.map_err(|e| format!("failed to read line {}: {}", linenumber, e))?;

        let data = line.trim();
        // Skip empty lines and comments.
        if data.is_empty() || data.starts_with('#') {
            continue;
        }

        match data.parse() {
            Ok(entry) => output.push(entry),
            Err(SdError { kind, msg }) if kind == ErrorKind::SysusersUnknownType => {
                log::warn!("skipped line {}: {}", linenumber, msg);
            }
            Err(e) => {
                let msg = format!(
                    "failed to parse sysusers entry at line {}: {}",
                    linenumber, e.msg
                );
                return Err(msg.into());
            }
        };
    }

    Ok(output)
}

impl FromStr for SysusersEntry {
    type Err = SdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use crate::errors::ErrorKind;

        let input = s.trim();
        match input.chars().next() {
            Some('g') => CreateGroup::from_str(input).map(|v| v.into_sysusers_entry()),
            Some('m') => AddUserToGroup::from_str(input).map(|v| v.into_sysusers_entry()),
            Some('r') => AddRange::from_str(input).map(|v| v.into_sysusers_entry()),
            Some('u') => CreateUserAndGroup::from_str(input).map(|v| v.into_sysusers_entry()),
            Some(t) => {
                let unknown = SdError {
                    kind: ErrorKind::SysusersUnknownType,
                    msg: format!("unknown sysusers type signature '{}'", t),
                };
                Err(unknown)
            }
            None => Err("missing sysusers type signature".into()),
        }
    }
}

impl FromStr for AddRange {
    type Err = SdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let data = parse_to_sysusers_data(s)?;
        data.try_into()
    }
}

impl FromStr for AddUserToGroup {
    type Err = SdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let data = parse_to_sysusers_data(s)?;
        data.try_into()
    }
}

impl FromStr for CreateGroup {
    type Err = SdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let data = parse_to_sysusers_data(s)?;
        data.try_into()
    }
}

impl FromStr for CreateUserAndGroup {
    type Err = SdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let data = parse_to_sysusers_data(s)?;
        data.try_into()
    }
}

/// Parse the content of a sysusers entry as `SysusersData`.
fn parse_to_sysusers_data(line: &str) -> Result<SysusersData, SdError> {
    let (rest, data) = parse_line(line).finish().map_err(|e| {
        format!(
            "parsing failed due to '{}' at '{}'",
            e.code.description(),
            e.input
        )
    })?;
    if !rest.is_empty() {
        return Err(format!("invalid trailing data: '{}'", rest).into());
    }
    Ok(data)
}

fn parse_line(input: &str) -> IResult<&str, SysusersData> {
    let rest = input;
    let (rest, kind) = {
        let (rest, kind) = anychar(rest)?;
        let (rest, _) = multispace1(rest)?;
        (rest, kind.to_string())
    };
    let (rest, name) = {
        let (rest, name) = take_while1(|c: char| !c.is_ascii_whitespace())(rest)?;
        let (rest, _) = multispace1(rest)?;
        (rest, name.to_string())
    };
    let (rest, id) = {
        let (rest, id) = take_while1(|c: char| !c.is_ascii_whitespace())(rest)?;
        let (rest, _) = multispace0(rest)?;
        (rest, id.to_string())
    };
    let (rest, gecos) = {
        let (rest, gecos) = parse_opt_string(rest)?;
        let (rest, _) = multispace0(rest)?;
        (rest, gecos.map(|s| s.to_string()))
    };
    let (rest, home_dir) = {
        let (rest, home_dir) = parse_opt_string(rest)?;
        let (rest, _) = multispace0(rest)?;
        (rest, home_dir.map(|s| s.to_string()))
    };
    let (rest, shell) = {
        let (rest, shell) = parse_opt_string(rest)?;
        let (rest, _) = multispace0(rest)?;
        (rest, shell.map(|s| s.to_string()))
    };

    let data = SysusersData {
        kind,
        name,
        id,
        gecos,
        home_dir,
        shell,
    };
    Ok((rest, data))
}

fn parse_opt_string(input: &str) -> IResult<&str, Option<&str>> {
    match input.chars().next() {
        None => Ok((input, None)),
        Some('"') => parse_quoted_string(input),
        _ => parse_plain_string(input),
    }
}

// XXX(lucab): should this account for inner escaped quotes?
fn parse_quoted_string(input: &str) -> IResult<&str, Option<&str>> {
    let rest = input;
    let (rest, _) = tag("\"")(rest)?;
    let (rest, txt) = take_while1(|c: char| c != '"')(rest)?;
    let (rest, _) = tag("\"")(rest)?;
    Ok((rest, Some(txt)))
}

fn parse_plain_string(input: &str) -> IResult<&str, Option<&str>> {
    let rest = input;
    let (rest, txt) = take_while1(|c: char| !c.is_ascii_whitespace())(rest)?;
    Ok((rest, Some(txt)))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::sysusers;

    #[test]
    fn test_type_g() {
        {
            let input = r#"g input - - - -"#;
            let expected = CreateGroup::new("input".to_string()).unwrap();
            let output: CreateGroup = input.parse().unwrap();
            assert_eq!(output, expected);
            let line = output.to_string();
            assert_eq!(line, input);
        }
    }

    #[test]
    fn test_type_m() {
        {
            let input = r#"m _authd input - - -"#;
            let expected = AddUserToGroup::new("_authd".to_string(), "input".to_string()).unwrap();
            let output: AddUserToGroup = input.parse().unwrap();
            assert_eq!(output, expected);
            let line = output.to_string();
            assert_eq!(line, input);
        }
    }

    #[test]
    fn test_type_r() {
        {
            let input = r#"r - 500-900 - - -"#;
            let expected = AddRange::new(500, 900).unwrap();
            let output: AddRange = input.parse().unwrap();
            assert_eq!(output, expected);
            let line = output.to_string();
            assert_eq!(line, input);
        }
    }

    #[test]
    fn test_type_u() {
        {
            let input =
                r#"u postgres - "Postgresql Database" /var/lib/pgsql /usr/libexec/postgresdb"#;
            let expected = CreateUserAndGroup::new(
                "postgres".to_string(),
                "Postgresql Database".to_string(),
                Some("/var/lib/pgsql".to_string().into()),
                Some("/usr/libexec/postgresdb".to_string().into()),
            )
            .unwrap();
            let output: CreateUserAndGroup = input.parse().unwrap();
            assert_eq!(output, expected);
            let line = output.to_string();
            assert_eq!(line, input);
        }
    }

    #[test]
    fn test_parse_from_reader() {
        let config_fragment = r#"
#Type Name     ID             GECOS                 Home directory Shell
u     httpd    404            "HTTP User"
u     _authd   /usr/bin/authd "Authorization user"
# Test comment
u     postgres -              "Postgresql Database" /var/lib/pgsql /usr/libexec/postgresdb
g     input    -              -

m     _authd   input
u     root     0              "Superuser"           /root          /bin/zsh
r     -        500-900
"#;

        let mut reader = config_fragment.as_bytes();
        let entries = sysusers::parse_from_reader(&mut reader).unwrap();
        assert_eq!(entries.len(), 7);
    }
}
