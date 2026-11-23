//! Command-line orchestration and process entry points.

mod acp;
mod args;
mod headless;
mod report;
mod sessions;
mod startup;
mod terminal;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use args::Args;
use startup::Startup;

pub(crate) fn run() -> Result<()> {
    let args = Args::parse();

    #[cfg(windows)]
    solaris_tools::cygwin::environment().map_err(anyhow::Error::msg)?;

    if !args.print_config
        && let Some(command) = args.history_command()
    {
        return command.run(solaris::application::startup::config_dir().as_deref());
    }
    let mut startup = Startup::load(&args)?;
    if args.print_config {
        return report::print_config(&startup);
    }

    let (execution, ask_approval) = startup.execution(&args)?;
    if args.acp {
        return acp::serve_acp(startup, &args, execution, ask_approval);
    }
    let journal = sessions::prepare(&mut startup, &args)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();
    let mcp_config = startup.mcp_config(&args)?;
    let mcp = runtime.block_on(solaris_mcp::McpManager::connect(
        mcp_config,
        &startup.resource_paths.cwd,
    ));
    for tool in mcp.tools() {
        startup.registry.add(Arc::clone(tool));
    }
    if args.mcp_status {
        println!("{}", serde_json::to_string_pretty(&mcp.status)?);
        if mcp.status.iter().any(|server| !server.connected) {
            anyhow::bail!("one or more MCP servers failed");
        }
        return Ok(());
    }
    for server in &mcp.status {
        if !server.connected {
            eprintln!("MCP {}: {}", server.name, server.message);
        }
    }

    for entry in startup.registry.unmatched(&startup.tools) {
        eprintln!(
            "solaris: no tool named `{entry}` on this platform — available: {}",
            startup.registry.names().join(", ")
        );
    }

    match &args.print {
        Some(prompt) => headless::run(
            startup,
            &args,
            prompt,
            &runtime,
            execution,
            ask_approval,
            journal,
        ),
        None => terminal::run(
            startup,
            args.connection_overrides(),
            execution,
            ask_approval,
            journal,
        ),
    }
}
