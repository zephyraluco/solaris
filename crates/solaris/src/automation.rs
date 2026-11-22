//! Thin command-line adapters over the shared session runtime.

use std::io::Write;

use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};
use solaris_agent::{AgentEvent, Mode, RuntimeCommand, SessionRuntime};

pub async fn print_turn(
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
