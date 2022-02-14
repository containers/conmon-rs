use anyhow::{Context, Result};
use conmon::Server;

fn main() -> Result<()> {
    Server::new()
        .context("create server")?
        .start()
        .context("start server")
}
