//! Generic version information for conmon

use anyhow::{Context, Result};
use serde::Serialize;

/// The version structure.
#[derive(Debug, Default, Eq, PartialEq, Serialize)]
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
            version: env!("CARGO_PKG_VERSION"),
            tag: env!("BUILD_TAG"),
            commit: env!("BUILD_COMMIT"),
            build_date: env!("BUILD_TIME"),
            target: env!("BUILD_TARGET"),
            rust_version: env!("BUILD_RUST_VERSION"),
            cargo_version: env!("BUILD_CARGO_VERSION"),
            cargo_tree: if verbose {
                env!("BUILD_CARGO_TREE")
            } else {
                ""
            },
        }
    }

    /// Whether verbose output is enabled.
    pub fn verbose(&self) -> bool {
        self.verbose
    }

    /// The current crate version.
    pub fn version(&self) -> &'static str {
        self.version
    }

    /// The tag of the build.
    pub fn tag(&self) -> &'static str {
        self.tag
    }

    /// The git commit SHA.
    pub fn commit(&self) -> &'static str {
        self.commit
    }

    /// The build date string.
    pub fn build_date(&self) -> &'static str {
        self.build_date
    }

    /// The target triple.
    pub fn target(&self) -> &'static str {
        self.target
    }

    /// The Rust version used.
    pub fn rust_version(&self) -> &'static str {
        self.rust_version
    }

    /// The Cargo version used.
    pub fn cargo_version(&self) -> &'static str {
        self.cargo_version
    }

    /// The cargo dependency tree.
    pub fn cargo_tree(&self) -> &'static str {
        self.cargo_tree
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
        assert_eq!(v.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(v.tag(), env!("BUILD_TAG"));
        assert_eq!(v.commit(), env!("BUILD_COMMIT"));
        assert_eq!(v.build_date(), env!("BUILD_TIME"));
        assert_eq!(v.target(), env!("BUILD_TARGET"));
        assert_eq!(v.rust_version(), env!("BUILD_RUST_VERSION"));
        assert_eq!(v.cargo_version(), env!("BUILD_CARGO_VERSION"));
        assert!(v.cargo_tree().is_empty());

        v.print();
    }

    #[test]
    fn version_test_verbose() {
        let v = Version::new(true);
        assert_eq!(v.cargo_tree(), env!("BUILD_CARGO_TREE"));
    }

    #[test]
    fn version_test_json() -> Result<()> {
        let v = Version::new(false);
        assert_eq!(v.version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(v.tag(), env!("BUILD_TAG"));
        assert_eq!(v.commit(), env!("BUILD_COMMIT"));
        assert_eq!(v.build_date(), env!("BUILD_TIME"));
        assert_eq!(v.target(), env!("BUILD_TARGET"));
        assert_eq!(v.rust_version(), env!("BUILD_RUST_VERSION"));
        assert_eq!(v.cargo_version(), env!("BUILD_CARGO_VERSION"));
        assert!(v.cargo_tree().is_empty());

        v.print_json()?;
        Ok(())
    }
}
