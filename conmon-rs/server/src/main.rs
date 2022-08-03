use anyhow::{Context, Result};
use conmonrs::Server;

fn main() -> Result<()> {
    Server::new()
        .context("create server")?
        .start()
        .context("start server")
}
