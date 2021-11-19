use anyhow::{Context, Result};
use conmon_server::Server;

fn main() -> Result<()> {
    Server::new()
        .context("create server")?
        .start()
        .context("start server")
}
