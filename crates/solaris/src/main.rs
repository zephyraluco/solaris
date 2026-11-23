//! Solaris process entry point.

mod acp;
mod cli;

fn main() -> anyhow::Result<()> {
    cli::run()
}

#[cfg(test)]
#[path = "../../solaris-agent/tests/support/mod.rs"]
mod test_support;
