//! Version information of conmonrs, for `--version` and the `version` RPC.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::{self, Write};

/// The dependency tree of the build, written by the build script.
const CARGO_TREE: &str = include_str!(concat!(env!("OUT_DIR"), "/cargo-tree.txt"));

/// The version structure.
#[derive(Debug, Default, Eq, PartialEq, Serialize)]
pub struct Version {
    /// Specifies if the output should contain verbose debug information.
    #[serde(skip)]
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

    /// The optional features this build supports, which the version alone
    /// does not tell.
    features: &'static [&'static str],
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
            cargo_tree: if verbose { CARGO_TREE } else { "" },
            features: Self::FEATURES,
        }
    }

    /// The relay of sd_notify messages requested by `sdnotifySocket`.
    pub const FEATURE_SDNOTIFY: &'static str = "sdnotify";

    /// Forwarding ports by `servePortForwardContainer`.
    pub const FEATURE_PORT_FORWARD: &'static str = "portforward";

    /// The `idleTimeoutSec` of the exec and attach serve requests, which
    /// older servers ignore. Port forwarding always applies its own.
    pub const FEATURE_IDLE_TIMEOUT: &'static str = "idletimeout";

    /// Exec and attach sessions over SPDY and the WebSocket protocols before
    /// `v5.channel.k8s.io`.
    pub const FEATURE_STREAMING_PROTOCOLS: &'static str = "streamingprotocols";

    /// The `containerName`, `tag` and `labels` of the journald log driver,
    /// which older servers ignore.
    pub const FEATURE_JOURNALD_FIELDS: &'static str = "journaldfields";

    const FEATURES: &'static [&'static str] = &[
        Self::FEATURE_SDNOTIFY,
        Self::FEATURE_PORT_FORWARD,
        Self::FEATURE_IDLE_TIMEOUT,
        Self::FEATURE_STREAMING_PROTOCOLS,
        Self::FEATURE_JOURNALD_FIELDS,
    ];

    /// The optional features this build supports.
    pub fn features(&self) -> &'static [&'static str] {
        self.features
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

    /// The version information as text, one field per line.
    fn text(&self) -> String {
        let tag = if self.tag().is_empty() {
            "none"
        } else {
            self.tag()
        };
        let mut text = format!(
            "version: {}\ntag: {tag}\ncommit: {}\nbuild: {}\ntarget: {}\n{}\n{}\nfeatures: {}\n",
            self.version(),
            self.commit(),
            self.build_date(),
            self.target(),
            self.rust_version(),
            self.cargo_version(),
            self.features().join(", "),
        );
        if self.verbose() {
            text.push_str(&format!("\ncargo tree: {}\n", self.cargo_tree()));
        }
        text
    }

    /// Print the version information to stdout.
    pub fn print(&self) -> Result<()> {
        write_output(io::stdout().lock(), &self.text())
    }

    /// Print the version information as JSON to stdout.
    pub fn print_json(&self) -> Result<()> {
        let mut json = serde_json::to_string(&self).context("serialize result")?;
        json.push('\n');
        write_output(io::stdout().lock(), &json)
    }
}

/// Write the output of a command. A reader which is gone, like `head`, ends
/// the output without an error, `println!` would panic and abort.
fn write_output(mut writer: impl Write, text: &str) -> Result<()> {
    match writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush())
    {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e).context("write output"),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_fields_and_verbosity() {
        let v = Version::new(false);
        assert_eq!(v.version(), env!("CARGO_PKG_VERSION"));
        assert!(!v.target().is_empty());
        assert!(v.rust_version().starts_with("rustc "));
        assert!(v.cargo_version().starts_with("cargo "));
        // The tree is only part of the verbose output.
        assert!(v.cargo_tree().is_empty());
        assert_eq!(Version::new(true).cargo_tree(), CARGO_TREE);

        let text = v.text();
        assert!(text.starts_with(&format!("version: {}\n", v.version())));
        assert!(text.ends_with(&format!("features: {}\n", v.features().join(", "))));
        assert!(Version::new(true).text().contains("\ncargo tree: "));
    }

    /// A writer which fails every write with `kind`.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn output_to_a_closed_pipe_is_no_error() {
        assert!(write_output(Failing(io::ErrorKind::BrokenPipe), "version").is_ok());
        assert!(write_output(Failing(io::ErrorKind::PermissionDenied), "version").is_err());
    }

    /// The JSON keys are what the Go client reads, see pkg/client/version.go.
    #[test]
    fn version_json() -> Result<()> {
        let v = Version::new(false);
        let json = serde_json::to_value(&v)?;
        let object = json.as_object().context("not an object")?;
        for (key, value) in [
            ("build_date", v.build_date()),
            ("cargo_tree", v.cargo_tree()),
            ("cargo_version", v.cargo_version()),
            ("commit", v.commit()),
            ("rust_version", v.rust_version()),
            ("tag", v.tag()),
            ("target", v.target()),
            ("version", v.version()),
        ] {
            assert_eq!(
                object.get(key).and_then(|v| v.as_str()),
                Some(value),
                "key {key}"
            );
        }
        // The verbosity is a presentation flag, not version information.
        assert!(!object.contains_key("verbose"));
        Ok(())
    }

    /// The Go client matches the names, see the `Feature` constants in
    /// pkg/client/version.go.
    #[test]
    fn version_features() -> Result<()> {
        let v = Version::new(false);
        assert_eq!(
            v.features(),
            [
                "sdnotify",
                "portforward",
                "idletimeout",
                "streamingprotocols",
                "journaldfields"
            ]
        );
        let json = serde_json::to_value(&v)?;
        assert_eq!(json.get("features"), Some(&serde_json::json!(v.features())));
        Ok(())
    }
}
