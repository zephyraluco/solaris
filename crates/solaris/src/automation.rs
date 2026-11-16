//! Thin command-line adapters over the shared session runtime.

use std::io::Write;

use anyhow::Result;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use solaris_agent::{AgentEvent, Mode, RuntimeCommand, RuntimeEventStream, SessionRuntime};
use tokio::io::AsyncRead;
use tokio_util::codec::{FramedRead, LinesCodec};

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
    let mut active: Option<RuntimeEventStream> = None;
    let mut request_id: Option<String> = None;
    let mut request_task = None;
    let mut last_sequence = 0;
    let mut pending_end: Option<u64> = None;
    let mut needs_terminal = false;
    let mut saw_terminal = false;
    let mut events = session.subscribe_from(session.snapshot().sequence);
    session.activate().map_err(anyhow::Error::msg)?;
    loop {
        if pending_end.is_some_and(|sequence| events.sequence() >= sequence)
            && (!needs_terminal || saw_terminal)
        {
            send(
                &mut output,
                &json!({"version":1,"type":"end","request_id":request_id.take()}),
            )?;
            request_task = None;
            pending_end = None;
            needs_terminal = false;
            saw_terminal = false;
        }
        tokio::select! {
            event = events.next() => {
                let event = event.map_err(anyhow::Error::msg)?;
                if event.task_id==request_task && event.event.is_terminal(){saw_terminal=true;}
                let mut envelope = event_envelope(&event);
                envelope["type"] = json!("event");
                envelope["request_id"] = if event.task_id == request_task {
                    json!(request_id)
                } else { Value::Null };
                send(&mut output, &envelope)?;
            }
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
                if command["type"]=="jobs" {
                    send(&mut output,&json!({"version":1,"type":"response","id":request.id,"ok":true,"jobs":session.jobs().list(&session.snapshot().session_id)}))?;
                    continue;
                }
                if command["type"]=="send_job" {
                    let result=match (command["id"].as_str(),command["text"].as_str()) {
                        (Some(id),Some(text))=>session.jobs().send(&session.snapshot().session_id,id,text),
                        _=>Err("send_job requires id and text".into()),
                    };
                    send(&mut output,&json!({"version":1,"type":"response","id":request.id,"ok":result.is_ok(),"error":result.err()}))?;
                    continue;
                }
                if command["type"]=="cancel_job" {command["type"]="cancel_task".into();}
                if command["type"] == "prompt" && command.get("mode").is_none() {
                    command["mode"] = serde_json::to_value(default_mode)?;
                }
                let result = match serde_json::from_value::<RuntimeCommand>(command) {
                    Err(error) => Err(error.to_string()),
                    Ok(command) if matches!(command, RuntimeCommand::Prompt { .. } | RuntimeCommand::Compact) && (active.is_some() || pending_end.is_some()) => Err("a prompt is already running".into()),
                    Ok(command) => {
                        let streaming = matches!(command, RuntimeCommand::Prompt { .. } | RuntimeCommand::Compact);
                        match match session.submit(command) { Ok(receipt)=>receipt.accepted_with_snapshot().await,Err(error)=>Err(error)} {
                            Ok((accepted,stream)) => {
                                if streaming {
                                    active = Some(stream);
                                    request_id = Some(request.id.clone());
                                    request_task = accepted.task_id;
                                    last_sequence=events.sequence();saw_terminal=false;needs_terminal=false;
                                }
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    }
                };
                send(&mut output, &json!({"version":1,"type":"response","id":request.id,"ok":result.is_ok(),"error":result.err()}))?;
            }
            event = async { match active.as_mut() {
                Some(stream) => stream.next().await,
                None => std::future::pending().await,
            }} => {
                if let Some(event)=event {
                    last_sequence=event.sequence.unwrap_or(last_sequence);
                    needs_terminal=event.sequence.is_none() && event.event.is_terminal();
                } else {
                    active=None;pending_end=Some(last_sequence);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_agent::AgentEventStream;
    use solaris_agent::testing::TurnRequest;
    use solaris_agent::{
        Agent, AgentOptions, BackendError, ConnectionConfig, ModelConnector, ToolRegistry,
    };
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    struct Waiting;
    #[async_trait::async_trait]
    impl ModelConnector for Waiting {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_agent::ModelClient, solaris_agent::BackendError> {
            Ok(solaris_agent::testing::client(
                self.capabilities(),
                move |request| {
                    let fixture = std::sync::Arc::clone(&self);
                    async move { fixture.respond(request).await }
                },
            ))
        }
    }
    impl Waiting {
        async fn respond(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            std::future::pending().await
        }
    }

    fn session() -> SessionRuntime {
        SessionRuntime::new(Agent::new(
            ConnectionConfig {
                backend: Arc::new(Waiting),
                provider_id: None,
                model: "test".into(),
                fallback_context_window: 128_000,
                instructions: None,
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
    async fn rpc_publishes_autonomous_input_events_without_a_prompt_request() {
        let session = SessionRuntime::new(Agent::new(
            ConnectionConfig {
                backend: Arc::new(Waiting),
                provider_id: None,
                model: "test".into(),
                fallback_context_window: 128_000,
                instructions: None,
            },
            Arc::new(ToolRegistry::with_tools(vec![])),
            AgentOptions::default(),
        ));
        let (input, mut client) = tokio::io::duplex(4096);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            serve_rpc(
                &session,
                Mode::Build,
                input,
                Output {
                    bytes: Vec::new(),
                    tx,
                },
            )
            .await
        });
        client
            .write_all(
                b"{\"id\":\"queued\",\"command\":{\"type\":\"queue\",\"text\":\"follow up\"}}\n",
            )
            .await
            .unwrap();
        assert_eq!(receive(&mut rx).await["ok"], true);
        let event = receive(&mut rx).await;
        assert_eq!(event["type"], "event");
        assert!(event["request_id"].is_null());
        assert!(event["sequence"].is_number());
        drop(client);
        server.await.unwrap().unwrap();
    }
    struct Immediate;
    #[async_trait::async_trait]
    impl ModelConnector for Immediate {
        async fn model_client(self: Arc<Self>) -> Result<solaris_agent::ModelClient, BackendError> {
            Ok(solaris_agent::testing::client(
                Default::default(),
                |_| async {
                    Ok(Box::pin(futures::stream::iter([
                        AgentEvent::TextDelta("answer".into()),
                        AgentEvent::TurnComplete {
                            usage: Default::default(),
                            cost_usd: 0.0,
                        },
                    ])) as AgentEventStream)
                },
            ))
        }
    }
    #[tokio::test]
    async fn rpc_fast_completion_keeps_request_identity_and_sends_end_after_events() {
        let session = SessionRuntime::new(Agent::new(
            ConnectionConfig {
                backend: Arc::new(Immediate),
                provider_id: None,
                model: "test".into(),
                fallback_context_window: 128_000,
                instructions: None,
            },
            Arc::new(ToolRegistry::with_tools(Vec::new())),
            AgentOptions::default(),
        ));
        let (input, mut client) = tokio::io::duplex(4096);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            serve_rpc(
                &session,
                Mode::Build,
                input,
                Output {
                    bytes: Vec::new(),
                    tx,
                },
            )
            .await
        });
        for turn in 0..10 {
            let id = format!("request-{turn}");
            client
                .write_all(
                    format!(
                        "{}\n",
                        json!({"id":id,"command":{"type":"prompt","text":"hello"}})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut complete = false;
            let mut answer = false;
            loop {
                let value = receive(&mut rx).await;
                match value["type"].as_str() {
                    Some("event") => {
                        assert_eq!(value["request_id"], id);
                        let payload: AgentEvent =
                            serde_json::from_value(value["event"].clone()).unwrap();
                        answer |= matches!(&payload,AgentEvent::TextDelta(text) if text.contains("answer"));
                        complete |= matches!(payload, AgentEvent::TurnComplete { .. });
                    }
                    Some("end") => {
                        assert_eq!(value["request_id"], id);
                        assert!(complete && answer);
                        break;
                    }
                    _ => {
                        assert_eq!(value["id"], id);
                        assert_eq!(value["ok"], true);
                    }
                }
            }
        }
        drop(client);
        server.await.unwrap().unwrap();
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
