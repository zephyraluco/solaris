//! Solaris process entry point.

mod acp;
mod cli;

fn main() -> anyhow::Result<()> {
    cli::run()
}
