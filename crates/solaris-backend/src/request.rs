//! One retry/compatibility policy over official model adapters, for every caller.
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use rig_core::effect::EffectKind;
use rig_core::error::ErrorReport;
use rig_core::serve::{Dispatch, ErasedHandler, Reply, adapters::ModelAdapter};
use rig_core::streaming::{Relayed, StreamEvents};

use crate::{Wire, http, policy::Repair};

pub(crate) fn execute(
    adapter: ErasedHandler,
    backend: crate::Backend,
    kind: EffectKind,
    dispatch: Dispatch,
    status: crate::connection::StatusObservers,
) -> Reply {
    let state = Request {
        adapter,
        backend,
        kind,
        dispatch,
        status,
        stream: None,
        delivered: false,
        done: false,
        retries: 0,
        repairs: Vec::new(),
    };
    Reply::Stream(Box::pin(futures::stream::unfold(
        state,
        |mut state| async move { state.next().await.map(|event| (event, state)) },
    )))
}
struct Request {
    backend: crate::Backend,
    adapter: ErasedHandler,
    kind: EffectKind,
    dispatch: Dispatch,
    status: crate::connection::StatusObservers,
    stream: Option<StreamEvents>,
    delivered: bool,
    done: bool,
    retries: usize,
    repairs: Vec<Repair>,
}
impl Request {
    async fn next(&mut self) -> Option<Result<Relayed, ErrorReport>> {
        loop {
            if self.done {
                return None;
            }
            if self.stream.is_none() {
                self.stream = Some(
                    self.adapter
                        .handle(self.kind.clone(), {
                            let mut dispatch =
                                Dispatch::new(self.dispatch.id(), self.dispatch.is_stream());
                            for scope in self.dispatch.scopes() {
                                dispatch = dispatch.with_scope(scope);
                            }
                            if let Some(context) = self.dispatch.adapter_context() {
                                dispatch = dispatch.with_adapter_context(context);
                            }
                            dispatch
                        })
                        .await
                        .into_stream(),
                );
            }
            match self.stream.as_mut()?.next().await {
                Some(Ok(item)) => {
                    // Any decoded content or unknown payload may contain output or
                    // usage. Never replay after it, even if it was not visible in UI.
                    self.delivered |= !matches!(item, Relayed::Origin(_));
                    self.done = matches!(item, Relayed::Done(_));
                    return Some(Ok(item));
                }
                Some(Err(error)) => {
                    if !self.delivered
                        && error.http_status == Some(400)
                        && self.backend.wire() == Wire::OpenAiChat
                        && let Some(repair) = crate::policy::repair_for(
                            error.provider_response_body().unwrap_or_default(),
                        )
                        && !self.repairs.contains(&repair)
                    {
                        self.repairs.push(repair);
                        match self.backend.completion_model(&self.repairs) {
                            Ok(model) => {
                                self.adapter =
                                    ErasedHandler::new(ModelAdapter::new("default", model));
                                self.stream = None;
                                continue;
                            }
                            Err(error) => {
                                self.done = true;
                                return Some(Err(ErrorReport::new(
                                    rig_core::error::ErrorKind::Request,
                                    error.to_string(),
                                )));
                            }
                        }
                    }
                    if !self.delivered && error.is_retryable() && self.retries < http::max_retries()
                    {
                        let wait = error
                            .retry_after(SystemTime::now())
                            .unwrap_or_default()
                            .max(http::backoff(self.retries, None))
                            .min(Duration::from_secs(30));
                        self.retries += 1;
                        self.stream = None;
                        let observers = self
                            .status
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        for observer in observers {
                            observer(format!(
                                "{} is busy — retrying in {:.1}s",
                                self.backend.endpoint.name,
                                wait.as_secs_f32()
                            ));
                        }
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    self.done = true;
                    let mut error = error;
                    error.message = self.backend.describe_error(&error).to_string();
                    return Some(Err(error));
                }
                None => {
                    self.done = true;
                    return Some(Err(rig_core::serve::stream_truncated()));
                }
            }
        }
    }
}
