//! Generic version information for conmon

use getset::CopyGetters;
use shadow_rs::shadow;

shadow!(build);

#[derive(CopyGetters, Debug, Default, Eq, PartialEq)]
#[getset(get_copy = "pub")]
/// The version structure for conmon.
pub struct Version {
    /// The current crate version.
    version: &'static str,

    /// The tag of the build, empty if not available.
    tag: &'static str,

    /// The git commit SHA of the build.
    commit: &'static str,

    /// The build date string.
    build_date: &'static str,

    /// The used Rust version.
    rust_version: &'static str,
}

impl Version {
    /// Create a new Version instance.
    pub fn new() -> Self {
        Self {
            version: build::PKG_VERSION,
            tag: build::TAG,
            commit: build::COMMIT_HASH,
            build_date: build::BUILD_TIME,
            rust_version: build::RUST_VERSION,
        }
    }

    /// Print the version information to stdout.
    pub fn print(&self) {
        println!("version: {}", build::PKG_VERSION);
        println!(
            "tag: {}",
            if build::TAG.is_empty() {
                "none"
            } else {
                build::TAG
            }
        );
        println!("commit: {}", build::COMMIT_HASH);
        println!("build: {}", build::BUILD_TIME);
        println!("{}", build::RUST_VERSION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_test() {
        let v = Version::new();
        assert_eq!(v.version(), build::PKG_VERSION);
        assert_eq!(v.tag(), build::TAG);
        assert_eq!(v.commit(), build::COMMIT_HASH);
        assert_eq!(v.build_date(), build::BUILD_TIME);
        assert_eq!(v.rust_version(), build::RUST_VERSION);

        v.print();
    }
}
