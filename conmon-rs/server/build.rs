use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

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

/// The first second of the year 10000, the build time has four digits for
/// the year.
const MAX_EPOCH: u64 = 253_402_300_800;

/// The UTC time of the seconds since the Unix epoch, formatted as
/// `YYYY-MM-DD hh:mm:ss`. The date is the one of the days since the epoch,
/// see <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>,
/// with the days shifted to start at 0000-03-01, so that the leap day is the
/// last one of a year.
fn format_utc(epoch: u64) -> String {
    let (days, secs) = (epoch / 86_400, epoch % 86_400);
    let days = days + 719_468;
    let (era, doe) = (days / 146_097, days % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        secs / 3_600,
        secs / 60 % 60,
        secs % 60
    )
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

    // Packagers build from tarballs without git metadata, or from a git
    // checkout of their packaging, so they can pass the tag and the commit
    // in. Either one being set wins over git, the other one stays empty.
    // In a worktree `.git` is a file pointing elsewhere, so ask git where
    // HEAD and the refs are. Tracking a path which does not exist would
    // rerun this script on every build.
    let env = |key| std::env::var(key).ok().filter(|v: &String| !v.is_empty());
    let (tag, commit) = match (env("BUILD_TAG"), env("BUILD_COMMIT")) {
        (None, None) if Path::new("../../.git").exists() => {
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
        }
        (tag, commit) => (tag.unwrap_or_default(), commit.unwrap_or_default()),
    };

    // Honor SOURCE_DATE_EPOCH for reproducible builds.
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse().ok())
        .filter(|&epoch| epoch < MAX_EPOCH)
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the clock is after the Unix epoch")
                .as_secs()
        });
    let build_time = format_utc(epoch);

    // Report the toolchain cargo builds with, which is not necessarily the
    // one on PATH.
    let target = std::env::var("TARGET").unwrap_or_default();
    let rustc = env("RUSTC").unwrap_or_else(|| "rustc".into());
    let cargo = env("CARGO").unwrap_or_else(|| "cargo".into());
    let rust_version = run(&rustc, &["--version"]);
    let cargo_version = run(&cargo, &["--version"]);

    // The dependency tree is only informational (`--version full`) and the
    // slowest step of this script, let packagers skip it. It covers the
    // enabled features, a plain `cargo tree` only shows the default ones, and
    // only what ends up in the binary for the target. It must not resolve the
    // dependencies anew or go to the network.
    let cargo_tree = if std::env::var_os("CONMONRS_SKIP_CARGO_TREE").is_some() {
        String::new()
    } else {
        // The tree changes with the locked dependencies. The lock file of the
        // workspace exists unless it got packaged without one.
        let lock_file = Path::new("../../Cargo.lock");
        if lock_file.exists() {
            println!("cargo:rerun-if-changed={}", lock_file.display());
        }
        let features = enabled_features();
        run(
            &cargo,
            &[
                "tree",
                "--locked",
                "--offline",
                "--edges",
                "normal",
                "--target",
                &target,
                "--no-default-features",
                "--features",
                &features,
            ],
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
