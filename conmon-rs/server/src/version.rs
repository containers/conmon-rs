//! Generic version information for conmon

#![allow(clippy::uninlined_format_args)]
#![allow(clippy::needless_raw_string_hashes)]

use anyhow::{Context, Result};
use getset::CopyGetters;
use serde::Serialize;
use shadow_rs::shadow;

shadow!(build);

#[derive(CopyGetters, Debug, Default, Eq, PartialEq, Serialize)]
#[getset(get_copy = "pub")]
/// The version structure.
pub struct Version {
    /// Specifies if the output should contain verbose debug information.
    verbose: bool,

    /// The current crate version.
    version: &'static str,

    /// The tag of the build, empty if not available.
    tag: &'static str,

    /// The git commit SHA of the build.
    commit: &'static str,

    /// The build date string.
    build_date: &'static str,

    /// The target triple string.
    target: &'static str,

    /// The used Rust version.
    rust_version: &'static str,

    /// The used Cargo version.
    cargo_version: &'static str,

    /// The cargo dependency tree, only available in verbose output.
    cargo_tree: &'static str,
}

impl Version {
    /// Create a new Version instance.
    pub fn new(verbose: bool) -> Self {
        Self {
            verbose,
            version: build::PKG_VERSION,
            tag: build::TAG,
            commit: build::COMMIT_HASH,
            build_date: build::BUILD_TIME,
            target: build::BUILD_TARGET,
            rust_version: build::RUST_VERSION,
            cargo_version: build::CARGO_VERSION,
            cargo_tree: if verbose { build::CARGO_TREE } else { "" },
        }
    }

    /// Print the version information to stdout.
    pub fn print(&self) {
        println!("version: {}", self.version());
        println!(
            "tag: {}",
            if self.tag().is_empty() {
                "none"
            } else {
                self.tag()
            }
        );
        println!("commit: {}", self.commit());
        println!("build: {}", self.build_date());
        println!("target: {}", self.target());
        println!("{}", self.rust_version());
        println!("{}", self.cargo_version());

        if self.verbose() {
            println!("\ncargo tree: {}", self.cargo_tree());
        }
    }

    /// Print the version information as JSON to stdout.
    pub fn print_json(&self) -> Result<()> {
        println!(
            "{}",
            serde_json::to_string(&self).context("serialize result")?
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_test() {
        let v = Version::new(false);
        assert_eq!(v.version(), build::PKG_VERSION);
        assert_eq!(v.tag(), build::TAG);
        assert_eq!(v.commit(), build::COMMIT_HASH);
        assert_eq!(v.build_date(), build::BUILD_TIME);
        assert_eq!(v.target(), build::BUILD_TARGET);
        assert_eq!(v.rust_version(), build::RUST_VERSION);
        assert_eq!(v.cargo_version(), build::CARGO_VERSION);
        assert!(v.cargo_tree().is_empty());

        v.print();
    }

    #[test]
    fn version_test_verbose() {
        let v = Version::new(true);
        assert_eq!(v.cargo_tree(), build::CARGO_TREE);
    }

    #[test]
    fn version_test_json() -> Result<()> {
        let v = Version::new(false);
        assert_eq!(v.version(), build::PKG_VERSION);
        assert_eq!(v.tag(), build::TAG);
        assert_eq!(v.commit(), build::COMMIT_HASH);
        assert_eq!(v.build_date(), build::BUILD_TIME);
        assert_eq!(v.target(), build::BUILD_TARGET);
        assert_eq!(v.rust_version(), build::RUST_VERSION);
        assert_eq!(v.cargo_version(), build::CARGO_VERSION);
        assert!(v.cargo_tree().is_empty());

        v.print_json()?;
        Ok(())
    }
}
