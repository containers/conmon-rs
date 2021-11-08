use std::io::Error;

fn main() -> Result<(), Error> {
    tonic_build::compile_protos("proto/conmon.proto")
}
