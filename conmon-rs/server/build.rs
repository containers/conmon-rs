use anyhow::{format_err, Context, Result};
use libbpf_cargo::SkeletonBuilder;
use std::{env, path::PathBuf};

const SRC: &str = "src/bpf/pidwatch.bpf.c";

fn main() -> Result<()> {
    let out_dir = env::var_os("OUT_DIR").context("OUT_DIR must be set in build script")?;
    let skel_path = PathBuf::from(out_dir).join("pidwatch.skel.rs");

    SkeletonBuilder::new()
        .source(SRC)
        .build_and_generate(&skel_path)
        .context("build bpf and generate skeleton")?;

    println!("cargo:rerun-if-changed={}", SRC);

    shadow_rs::new().map_err(|e| format_err!("inject build-time variables: {:?}", e))
}
