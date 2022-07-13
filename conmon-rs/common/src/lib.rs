#![allow(clippy::all)]
pub mod conmon_capnp {
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}
