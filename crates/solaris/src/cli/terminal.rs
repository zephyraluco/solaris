use std::sync::Arc;

use anyhow::Result;
use solaris::application::history::ConnectionOverrides;
use solaris::tui::{greeting, tips};
use solaris::{App, AppOptions, BackendFactory, Clipboard};
use solaris_agent::{ExecutionSettings, SessionJournal};
use solaris_tui::Tui;

use super::Startup;

pub(super) fn run(
    startup: Startup,
    overrides: ConnectionOverrides,
    execution: ExecutionSettings,
    ask_approval: bool,
    journal: Option<Arc<SessionJournal>>,
) -> Result<()> {
    let Startup {
        resource_paths,
        registry,
        tools,
        auth,
        auth_path,
        backend_options,
        config,
        resources,
        user,
        buddy,
        buddy_path,
        recent,
        recent_path,
        settings,
        settings_path,
        ..
    } = startup;
    solaris_tui::install_panic_hook();
    let mut terminal = solaris_tui::setup_terminal()?;

    let mut tui = Tui::new();
    let factory: BackendFactory = Arc::new(solaris_provider::session_backend_factory(
        auth_path.clone(),
        backend_options,
    ));
    let app = App::new(AppOptions {
        backend_factory: factory,
        config,
        auth,
        auth_path,
        version: env!("CARGO_PKG_VERSION").to_string(),
        greeting: greeting(&user),
        buddy,
        buddy_path,
        tip: tips::select(recent.len()).content.to_string(),
        recent,
        recent_path,
        preferences: settings,
        settings_path,
        quit: tui.quit_flag(),
        overlay_queue: tui.overlay_queue(),
        overlay_flag: tui.overlay_flag(),
        selection: tui.selection(),
        clipboard: Clipboard::system(),
        // The binary wants the provider's own model list in the picker; the
        // catalogue is what it falls back to.
        discover_models: true,
        tools,
        skills: resources.skills,
        prompt_context: resources.prompt,
        resource_paths: Some(resource_paths),
        ask_approval,
        execution,
        registry: Some(registry),
        journal,
    })
    .with_connection_overrides(overrides);
    tui.set_root(Box::new(app));

    let outcome = tui.run(&mut terminal);

    // Always restore, even when the loop failed.
    solaris_tui::restore_terminal()?;
    outcome?;

    Ok(())
}
