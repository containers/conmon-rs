use std::{path::Path, process::Command};

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The features this build enables, as passed to `--features`. Cargo sets
/// `CARGO_FEATURE_<NAME>` for each, with the name upper cased and dashes
/// replaced by underscores.
fn enabled_features() -> String {
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| {
            key.strip_prefix("CARGO_FEATURE_")
                .map(|name| name.to_lowercase().replace('_', "-"))
        })
        .collect();
    features.sort();
    features.join(",")
}

fn main() {
    println!("cargo:rerun-if-changed=proto/conmon.capnp");
    capnpc::CompilerCommand::new()
        .file("proto/conmon.capnp")
        .run()
        .expect("compile the Cap'n Proto schema");

    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=CONMONRS_SKIP_CARGO_TREE");
    println!("cargo:rerun-if-env-changed=BUILD_TAG");
    println!("cargo:rerun-if-env-changed=BUILD_COMMIT");

    // Packagers build from tarballs without git metadata, so they can pass
    // the commit and tag in. In a worktree `.git` is a file pointing
    // elsewhere, so ask git where HEAD and the refs are. Tracking a path which
    // does not exist would rerun this script on every build.
    let env = |key| std::env::var(key).ok().filter(|v: &String| !v.is_empty());
    let (tag, commit) = if let Some(commit) = env("BUILD_COMMIT") {
        (env("BUILD_TAG").unwrap_or_default(), commit)
    } else if Path::new("../../.git").exists() {
        for (arg, file) in [("--git-dir", "HEAD"), ("--git-common-dir", "refs")] {
            let dir = run("git", &["rev-parse", "--path-format=absolute", arg]);
            let path = Path::new(&dir).join(file);
            if !dir.is_empty() && path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        (
            run("git", &["describe", "--tags", "--always", "--dirty"]),
            run("git", &["rev-parse", "HEAD"]),
        )
    } else {
        (String::new(), String::new())
    };

    // Honor SOURCE_DATE_EPOCH for reproducible builds.
    let build_time = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(epoch) => run(
            "date",
            &["-u", "-d", &format!("@{epoch}"), "+%Y-%m-%d %H:%M:%S"],
        ),
        Err(_) => run("date", &["-u", "+%Y-%m-%d %H:%M:%S"]),
    };

    let target = std::env::var("TARGET").unwrap_or_default();
    let rust_version = run("rustc", &["--version"]);
    let cargo_version = run("cargo", &["--version"]);

    // The dependency tree is only informational (`--version full`) and the
    // slowest step of this script, let packagers skip it. It covers the
    // enabled features, a plain `cargo tree` only shows the default ones.
    let cargo_tree = if std::env::var_os("CONMONRS_SKIP_CARGO_TREE").is_some() {
        String::new()
    } else {
        let features = enabled_features();
        run(
            "cargo",
            &["tree", "--no-default-features", "--features", &features],
        )
    };

    println!("cargo:rustc-env=BUILD_TAG={tag}");
    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_TIME={build_time}");
    println!("cargo:rustc-env=BUILD_TARGET={target}");
    println!("cargo:rustc-env=BUILD_RUST_VERSION={rust_version}");
    println!("cargo:rustc-env=BUILD_CARGO_VERSION={cargo_version}");
    // An environment variable holds a single line, so the multi line tree goes
    // into a file which the version module includes.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    std::fs::write(Path::new(&out_dir).join("cargo-tree.txt"), cargo_tree)
        .expect("write cargo tree");
}
