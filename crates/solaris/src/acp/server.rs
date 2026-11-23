//! ACP connection setup and request dispatch.

use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::*;
use agent_client_protocol::{Agent as AcpAgent, Stdio};
use solaris_agent::{Runtime, RuntimeCommand};
use tokio::sync::Mutex;

use super::session::{
    ClientConnection, Server, Settings, configure, failure, finish_prompt, invalid, start_prompt,
};

pub(crate) async fn serve(settings: Settings) -> anyhow::Result<()> {
    let server = Arc::new(Server {
        settings,
        runtime: Runtime::default(),
        sessions: Mutex::default(),
        forms: Default::default(),
    });
    let init = server.clone();
    let open = server.clone();
    let load = server.clone();
    let prompts = server.clone();
    let cancel = server.clone();
    let config = server.clone();
    let result = AcpAgent
        .v1()
        .name("solaris")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _| {
                init.forms.store(
                    request
                        .client_capabilities
                        .elicitation
                        .and_then(|caps| caps.form)
                        .is_some(),
                    std::sync::atomic::Ordering::Relaxed,
                );

                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(PromptCapabilities::new().image(true))
                            .mcp_capabilities(McpCapabilities::new().http(true)),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, connection| {
                let result = open
                    .open(&request.cwd, None, request.mcp_servers, &connection)
                    .await
                    .map(|(id, options)| NewSessionResponse::new(id).config_options(options));
                responder.respond_with_result(result)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                let result = load
                    .open(
                        &request.cwd,
                        Some(&request.session_id),
                        request.mcp_servers,
                        &connection,
                    )
                    .await
                    .map(|(_, options)| LoadSessionResponse::new().config_options(options));
                responder.respond_with_result(result)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ClientConnection| {
                let session = match prompts.session(&request.session_id).await {
                    Ok(session) => session,
                    Err(error) => return responder.respond_with_error(error),
                };
                let turn = match session.operation.clone().try_lock_owned() {
                    Ok(turn) => turn,
                    Err(_) => return responder.respond_with_error(invalid("session is busy")),
                };
                let id = request.session_id.clone();
                let stream = match start_prompt(&session, request).await {
                    Ok(stream) => stream,
                    Err(error) => return responder.respond_with_error(error),
                };
                let client = connection.clone();
                connection.spawn(async move {
                    let _turn = turn;
                    responder
                        .respond_with_result(finish_prompt(&session, &id, &client, stream).await)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _| {
                let result = match config.session(&request.session_id).await {
                    Ok(session) => configure(session, request).await,
                    Err(error) => Err(error),
                };
                responder.respond_with_result(result)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |request: CancelNotification, _| {
                if let Ok(session) = cancel.session(&request.session_id).await {
                    session
                        .runtime
                        .dispatch(RuntimeCommand::Cancel)
                        .map_err(failure)?;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;
    server.runtime.shutdown();
    result?;
    Ok(())
}
