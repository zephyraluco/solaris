//! Thin command-line adapters over the shared session runtime.

use std::io::Write;

use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use solaris_agent::{AgentEvent, AgentEventStream, Mode, RuntimeCommand, SessionRuntime};
use tokio::io::AsyncRead;
use tokio_util::codec::{FramedRead, LinesCodec};

pub async fn print_turn(
    session: &SessionRuntime,
    prompt: &str,
    mode: Mode,
    json_output: bool,
) -> Result<bool> {
    let mut stream = session.dispatch(RuntimeCommand::Prompt {
        text: prompt.into(),
        mode,
    });
    let mut sequence = 0u64;
    let mut succeeded = false;
    while let Some(event) = stream.next().await {
        sequence += 1;
        if json_output {
            println!("{}", event_envelope(session, sequence, &event));
        } else {
            match &event {
                AgentEvent::TextDelta(text) => {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
                AgentEvent::Error(error) => eprintln!("{error}"),
                _ => {}
            }
        }
        match event {
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

fn event_envelope(session: &SessionRuntime, sequence: u64, event: &AgentEvent) -> Value {
    json!({"version":1,"sequence":sequence,"session_id":session.journal().as_ref().map(|j| j.id().to_string()),"event":event})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    command: Value,
}

fn send(output: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// Each request gets a response; prompt events and a final end marker carry its ID.
/// A closed input cancels and drops the current turn. No additional server runs.
pub async fn serve_rpc(
    session: &SessionRuntime,
    default_mode: Mode,
    input: impl AsyncRead + Unpin,
    mut output: impl Write,
) -> Result<()> {
    let mut lines = FramedRead::new(input, LinesCodec::new_with_max_length(1024 * 1024));
    let mut active: Option<AgentEventStream> = None;
    let mut request_id: Option<String> = None;
    let mut sequence = 0u64;
    loop {
        tokio::select! {
            line = lines.next() => {
                let Some(line) = line else {
                    session.cancel_handle().cancel();
                    return Ok(());
                };
                let line = line?;
                let request = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        send(&mut output, &json!({"version":1,"type":"response","id":null,"ok":false,"error":error.to_string()}))?;
                        continue;
                    }
                };
                let mut command = request.command;
                if command["type"] == "prompt" && command.get("mode").is_none() {
                    command["mode"] = serde_json::to_value(default_mode)?;
                }
                let result = serde_json::from_value::<RuntimeCommand>(command)
                    .map_err(|error| error.to_string())
                    .and_then(|command| {
                        if matches!(command, RuntimeCommand::Prompt { .. }) {
                            if active.is_some() { return Err("a prompt is already running".into()); }
                            active = Some(session.dispatch(command));
                            request_id = Some(request.id.clone());
                        } else {
                            drop(session.dispatch(command));
                        }
                        Ok(())
                    });
                send(&mut output, &json!({"version":1,"type":"response","id":request.id,"ok":result.is_ok(),"error":result.err()}))?;
            }
            event = async { match active.as_mut() {
                Some(stream) => stream.next().await,
                None => std::future::pending().await,
            }} => {
                if let Some(event) = event {
                    sequence += 1;
                    let mut envelope = event_envelope(session, sequence, &event);
                    envelope["type"] = json!("event");
                    envelope["request_id"] = json!(request_id);
                    send(&mut output, &envelope)?;
                } else {
                    active = None;
                    send(&mut output, &json!({"version":1,"type":"end","request_id":request_id.take()}))?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::{
        Agent, AgentBackend, AgentOptions, BackendError, ToolRegistry, TurnRequest,
    };
    use solaris_provider::{BackendChoice, CredentialSource};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    struct Waiting;
    #[async_trait::async_trait]
    impl AgentBackend for Waiting {
        async fn run_turn(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            std::future::pending().await
        }
    }
    fn session() -> SessionRuntime {
        SessionRuntime::new(Agent::new(
            BackendChoice {
                backend: Arc::new(Waiting),
                provider_id: None,
                base_url: None,
                model: "test".into(),
                source: CredentialSource::Missing,
            },
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        ))
    }
    struct Output {
        bytes: Vec<u8>,
        tx: tokio::sync::mpsc::UnboundedSender<Value>,
    }
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let value = serde_json::from_slice(&self.bytes)?;
            self.bytes.clear();
            self.tx.send(value).map_err(std::io::Error::other)
        }
    }
    async fn receive(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("prompt RPC response")
            .expect("open RPC")
    }
    #[tokio::test]
    async fn rpc_recovers_from_bad_commands_and_controls_an_active_prompt() {
        let (input, mut client) = tokio::io::duplex(4096);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            serve_rpc(
                &session(),
                Mode::Plan,
                input,
                Output {
                    bytes: Vec::new(),
                    tx,
                },
            )
            .await
        });
        client.write_all(b"not json\n{\"id\":\"invalid\",\"command\":{\"type\":\"unknown\"}}\n{\"id\":\"one\",\"command\":{\"type\":\"prompt\",\"text\":\"hello\"}}\n").await.expect("commands");
        assert_eq!(receive(&mut rx).await["ok"], false);
        assert_eq!(receive(&mut rx).await["id"], "invalid");
        assert_eq!(receive(&mut rx).await["ok"], true);
        client.write_all(b"{\"id\":\"busy\",\"command\":{\"type\":\"prompt\",\"text\":\"second\"}}\n{\"id\":\"stop\",\"command\":{\"type\":\"cancel\"}}\n").await.expect("controls");
        let mut saw_busy = false;
        let mut saw_cancel = false;
        loop {
            let value = receive(&mut rx).await;
            assert_eq!(value["version"], 1);
            if value["id"] == "busy" {
                saw_busy = value["ok"] == false;
            }
            if value["id"] == "stop" {
                saw_cancel = value["ok"] == true;
            }
            if value["type"] == "end" {
                assert_eq!(value["request_id"], "one");
                break;
            }
        }
        assert!(saw_busy && saw_cancel);
        client
            .write_all(b"{\"id\":\"two\",\"command\":{\"type\":\"prompt\",\"text\":\"again\"}}\n")
            .await
            .expect("next turn");
        assert_eq!(receive(&mut rx).await["ok"], true);
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("EOF exits")
            .expect("join")
            .expect("RPC");
    }
    #[tokio::test]
    async fn rpc_rejects_oversized_input_before_parsing() {
        let bytes = vec![b'x'; 1024 * 1024 + 1];
        let error = serve_rpc(&session(), Mode::Build, bytes.as_slice(), std::io::sink())
            .await
            .expect_err("bounded line");
        assert!(error.to_string().contains("max line length"));
    }
}
