/// Unit name escaping, like `systemd-escape`.
pub fn escape_name(name: &str) -> String {
    if name.is_empty() {
        return "".to_string();
    }

    let parts: Vec<String> = name
        .bytes()
        .enumerate()
        .map(|(n, b)| escape_byte(b, n))
        .collect();
    parts.join("")
}

/// Path escaping, like `systemd-escape --path`.
pub fn escape_path(name: &str) -> String {
    let trimmed = name.trim_matches('/');
    if trimmed.is_empty() {
        return "-".to_string();
    }

    let mut slash_seq = false;
    let parts: Vec<String> = trimmed
        .bytes()
        .filter(|b| {
            let is_slash = *b == b'/';
            let res = !(is_slash && slash_seq);
            slash_seq = is_slash;
            res
        })
        .enumerate()
        .map(|(n, b)| escape_byte(b, n))
        .collect();
    parts.join("")
}

fn escape_byte(b: u8, index: usize) -> String {
    let c = char::from(b);
    match c {
        '/' => '-'.to_string(),
        ':' | '_' | '0'..='9' | 'a'..='z' | 'A'..='Z' => c.to_string(),
        '.' if index > 0 => c.to_string(),
        _ => format!(r#"\x{:02x}"#, b),
    }
}

#[cfg(test)]
mod test {
    use crate::unit::*;
    use quickcheck::quickcheck;

    quickcheck! {
        fn test_byte_escape_length(xs: u8, n: usize) -> bool {
            let out = escape_byte(xs, n);
            out.len() == 1 || out.len() == 4
        }
    }

    #[test]
    fn test_name_escape() {
        let cases = vec![
            // leave empty string empty
            (r#""#, r#""#),
            // escape leading dot
            (r#".foo/.bar"#, r#"\x2efoo-.bar"#),
            // escape disallowed
            (r#"///..\-!#??///"#, r#"---..\x5c\x2d\x21\x23\x3f\x3f---"#),
            // escape real-world example
            (
                r#"user-cloudinit@/var/lib/coreos/vagrant/vagrantfile-user-data.service"#,
                r#"user\x2dcloudinit\x40-var-lib-coreos-vagrant-vagrantfile\x2duser\x2ddata.service"#,
            ),
        ];

        for t in cases {
            let res = escape_name(t.0);
            assert_eq!(res, t.1.to_string());
        }
    }

    #[test]
    fn test_path_escape() {
        let cases = vec![
            // turn empty string path into escaped /
            (r#""#, r#"-"#),
            // turn redundant ////s into single escaped /
            (r#"/////////"#, r#"-"#),
            // remove all redundant ////s
            (r#"///foo////bar/////tail//////"#, r#"foo-bar-tail"#),
            // escape leading dot
            (r#"."#, r#"\x2e"#),
            (r#"/."#, r#"\x2e"#),
            (r#"/////////.///////////////"#, r#"\x2e"#),
            (r#"....."#, r#"\x2e...."#),
            (r#"/.foo/.bar"#, r#"\x2efoo-.bar"#),
            (r#".foo/.bar"#, r#"\x2efoo-.bar"#),
            // escape disallowed
            (r#"///..\-!#??///"#, r#"\x2e.\x5c\x2d\x21\x23\x3f\x3f"#),
        ];

        for t in cases {
            let res = escape_path(t.0);
            assert_eq!(res, t.1.to_string());
        }
    }

    quickcheck! {
        fn test_path_escape_nonempty(xs: String) -> bool {
            let out = escape_path(&xs);
            !out.is_empty()
        }
    }

    quickcheck! {
        fn test_path_escape_no_slash(xs: String) -> bool {
            let out = escape_path(&xs);
            !out.contains('/')
        }
    }

    quickcheck! {
        fn test_path_escape_no_dash_runs(xs: String) -> bool {
            let out = escape_path(&xs);
            !out.contains("--")
        }
    }

    quickcheck! {
        fn test_path_escape_no_leading_dot(xs: String) -> bool {
            let out = escape_path(&xs);
            !out.starts_with('.')
        }
    }
}
