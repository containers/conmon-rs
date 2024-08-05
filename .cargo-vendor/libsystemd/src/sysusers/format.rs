use super::*;
use std::fmt::{self, Display};

impl Display for SysusersEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SysusersEntry::AddRange(v) => write!(f, "{}", v),
            SysusersEntry::AddUserToGroup(v) => write!(f, "{}", v),
            SysusersEntry::CreateGroup(v) => write!(f, "{}", v),
            SysusersEntry::CreateUserAndGroup(v) => write!(f, "{}", v),
        }
    }
}

impl Display for AddRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r - {}-{} - - -", self.from, self.to)
    }
}

impl Display for AddUserToGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m {} {} - - -", self.username, self.groupname,)
    }
}

impl Display for CreateGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "g {} {} - - -", self.groupname, self.gid,)
    }
}

impl Display for CreateUserAndGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "u {} {} \"{}\" {} {}",
            self.name,
            self.id,
            self.gecos,
            self.home_dir
                .as_deref()
                .map(|p| p.to_string_lossy())
                .unwrap_or(Cow::Borrowed("-")),
            self.shell
                .as_deref()
                .map(|p| p.to_string_lossy())
                .unwrap_or(Cow::Borrowed("-")),
        )
    }
}

impl Display for IdOrPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdOrPath::Id(i) => write!(f, "{}", i),
            IdOrPath::UidGid((u, g)) => write!(f, "{}:{}", u, g),
            IdOrPath::UidGroupname((u, g)) => write!(f, "{}:{}", u, g),
            IdOrPath::Path(p) => write!(f, "{}", p.display()),
            IdOrPath::Automatic => write!(f, "-",),
        }
    }
}

impl Display for GidOrPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GidOrPath::Gid(g) => write!(f, "{}", g),
            GidOrPath::Path(p) => write!(f, "{}", p.display()),
            GidOrPath::Automatic => write!(f, "-",),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_formatters() {
        {
            let type_u =
                CreateUserAndGroup::new("foo0".to_string(), "test".to_string(), None, None)
                    .unwrap();
            let expected = r#"u foo0 - "test" - -"#;
            assert_eq!(type_u.to_string(), expected);
        }
        {
            let type_g = CreateGroup::new("foo1".to_string()).unwrap();
            let expected = r#"g foo1 - - - -"#;
            assert_eq!(type_g.to_string(), expected);
        }
        {
            let type_r = AddRange::new(10, 20).unwrap();
            let expected = r#"r - 10-20 - - -"#;
            assert_eq!(type_r.to_string(), expected);
        }
        {
            let type_m = AddUserToGroup::new("foo3".to_string(), "bar".to_string()).unwrap();
            let expected = r#"m foo3 bar - - -"#;
            assert_eq!(type_m.to_string(), expected);
        }
    }
}
