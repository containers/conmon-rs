use capnp::Error;
use capnpc::CompilerCommand;

fn main() -> Result<(), Error> {
    CompilerCommand::new().file("proto/conmon.capnp").run()
}
