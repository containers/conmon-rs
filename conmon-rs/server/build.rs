use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");

    let tag = run("git", &["describe", "--tags", "--always", "--dirty"]);
    let commit = run("git", &["rev-parse", "HEAD"]);
    let build_time = run("date", &["-u", "+%Y-%m-%d %H:%M:%S"]);
    let target = std::env::var("TARGET").unwrap_or_default();
    let rust_version = run("rustc", &["--version"]);
    let cargo_version = run("cargo", &["--version"]);
    let cargo_tree = run("cargo", &["tree"]);

    println!("cargo:rustc-env=BUILD_TAG={tag}");
    println!("cargo:rustc-env=BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=BUILD_TIME={build_time}");
    println!("cargo:rustc-env=BUILD_TARGET={target}");
    println!("cargo:rustc-env=BUILD_RUST_VERSION={rust_version}");
    println!("cargo:rustc-env=BUILD_CARGO_VERSION={cargo_version}");
    println!("cargo:rustc-env=BUILD_CARGO_TREE={cargo_tree}");
}
