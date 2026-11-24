//! Thin command-line adapters over the shared session runtime.

use std::{io::Write, sync::Arc};

use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};
use solaris::application::{execution::with_approval, session::build_agent};
use solaris_agent::{
    AgentEvent, ExecutionSettings, Mode, RuntimeCommand, SessionJournal, SessionRuntime,
};

use super::{Args, Startup};

pub(super) fn run(
    startup: Startup,
    args: &Args,
    prompt: &str,
    runtime: &tokio::runtime::Runtime,
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
        ..
    } = startup;
    let mut options =
        solaris_agent::AgentOptions::in_directory(resource_paths.cwd).with_selection(tools);
    options.execution = with_approval(execution, ask_approval);
    let factory = Arc::new(solaris_provider::refreshing_backend_factory(
        auth,
        auth_path,
        backend_options,
    ));
    let choice = factory(&config.model);
    // A noninteractive run cannot wait for an approval prompt.
    if ask_approval || config.mode == Mode::Ask {
        options.approver = Arc::new(solaris_agent::DenyAll);
    }
    let sessions = solaris_agent::Runtime::default();
    let session = sessions.open(build_agent(
        choice,
        Arc::new(registry),
        options,
        resources,
        move |model| factory(model),
    ))?;
    if let Some(journal) = journal {
        session.attach_journal(journal)?;
    }
    if args.image.len() > 8 {
        anyhow::bail!("at most eight images may be attached to one prompt");
    }
    for path in &args.image {
        let image = solaris_tools::read_image(path)?
            .ok_or_else(|| anyhow::anyhow!("{} is not a supported image", path.display()))?;
        session.attach_image(image).map_err(anyhow::Error::msg)?;
    }
    let succeeded = runtime.block_on(print_turn(&session, prompt, config.mode, args.json))?;
    if !succeeded {
        anyhow::bail!("headless turn failed");
    }
    Ok(())
}

async fn print_turn(
    session: &SessionRuntime,
    prompt: &str,
    mode: Mode,
    json_output: bool,
) -> Result<bool> {
    let mut stream = session
        .execute(RuntimeCommand::Prompt {
            text: prompt.into(),
            mode,
        })
        .await
        .map_err(anyhow::Error::msg)?;
    let mut succeeded = false;
    while let Some(event) = stream.next().await {
        if json_output {
            println!("{}", event_envelope(&event));
        } else {
            match &event.event {
                AgentEvent::TextDelta(text) => {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
                AgentEvent::Error(error) => eprintln!("{error}"),
                _ => {}
            }
        }
        match event.event {
            AgentEvent::TurnComplete { .. } => succeeded = true,
            AgentEvent::Error(_) => succeeded = false,
            _ => {}
        }
    }
    if !json_output {
        println!();
    }
    Ok(succeeded)
}

fn event_envelope(event: &solaris_agent::RuntimeEvent) -> Value {
    let mut envelope = serde_json::to_value(event).expect("runtime event serializes");
    envelope["version"] = json!(1);
    envelope
}
