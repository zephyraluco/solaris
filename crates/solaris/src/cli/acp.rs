use anyhow::Result;
use solaris_agent::ExecutionSettings;

use super::{Args, Startup, sessions};
use crate::acp;

pub(super) fn serve_acp(
    startup: Startup,
    args: &Args,
    execution: ExecutionSettings,
    ask_approval: bool,
) -> Result<()> {
    let store = sessions::acp_store(&startup)?;
    let mcp = startup.mcp_config(args)?;
    let Startup {
        resource_paths,
        registry,
        tools,
        auth,
        auth_path,
        backend_options,
        config,
        ..
    } = startup;
    tokio::runtime::Runtime::new()?.block_on(acp::serve(acp::Settings {
        store,
        resources: resource_paths,
        registry,
        selection: tools,
        execution,
        auth,
        auth_path,
        backend_options,
        model: config.model,
        mode: config.mode,
        overrides: args.connection_overrides(),
        confirm_tools: ask_approval,
        mcp,
    }))
}
