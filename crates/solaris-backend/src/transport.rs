//! Product request options through official HTTP middleware. Framing stays in Rig.
use rig_core::{
    driver::{Exchange, Opening, Transport},
    error::ProviderError,
    http_client::{
        Bytes, DynHttpClient, Exchange as HttpExchange, HeaderMap, HttpMiddleware, Result,
    },
    wasm_compat::WasmBoxedFuture,
    wire::{Encoded, Wire, WireFrame},
};

impl crate::Backend {
    pub(crate) fn request_http(&self, repairs: &[crate::policy::Repair]) -> DynHttpClient {
        DynHttpClient::new(rig_reqwest::ReqwestClient::from(self.client.clone())).with_middleware(
            RequestOptions {
                session_header: self.endpoint.session_header,
                session: super::client::session_id(),
                account: self.chatgpt.as_ref().map(|(account, _)| account.clone()),
                repairs: repairs.to_vec(),
            },
        )
    }
}
struct RequestOptions {
    session_header: Option<&'static str>,
    session: &'static str,
    account: Option<String>,
    repairs: Vec<crate::policy::Repair>,
}
impl HttpMiddleware for RequestOptions {
    fn before_request_headers<'a>(
        &'a self,
        _: HttpExchange<'a>,
        headers: &'a mut HeaderMap,
    ) -> WasmBoxedFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(name) = self.session_header {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(rig_core::http_client::Error::instance)?;
                headers.insert(
                    name,
                    reqwest::header::HeaderValue::from_static(self.session),
                );
            }
            if let Some(account) = &self.account {
                headers.insert(
                    "chatgpt-account-id",
                    account
                        .parse()
                        .map_err(rig_core::http_client::Error::instance)?,
                );
                headers.insert(
                    "originator",
                    reqwest::header::HeaderValue::from_static("solaris"),
                );
            }
            Ok(())
        })
    }
    fn before_request_body<'a>(
        &'a self,
        _: HttpExchange<'a>,
        _: &'a HeaderMap,
        bytes: Bytes,
    ) -> WasmBoxedFuture<'a, Result<Bytes>> {
        Box::pin(async move {
            if bytes.is_empty() || self.repairs.is_empty() {
                return Ok(bytes);
            }
            let mut body: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(rig_core::http_client::Error::instance)?;
            for repair in &self.repairs {
                crate::policy::apply_repair(&mut body, *repair);
            }
            Ok(serde_json::to_vec(&body)
                .map_err(rig_core::http_client::Error::instance)?
                .into())
        })
    }
}

// Only a gateway's exact completion URL needs overriding: the official Messages
// wire otherwise always appends /v1/messages to its normalized API root.
// Headers, body policies, HTTP, framing and response decoding remain official.
#[derive(Clone)]
pub(crate) struct CompletionRoute {
    http: DynHttpClient,
    url: String,
}
impl CompletionRoute {
    pub fn new(http: DynHttpClient, url: String) -> Self {
        Self { http, url }
    }
}
impl<W: Wire<Payload = Encoded, Frame = WireFrame>> Transport<W> for CompletionRoute {
    fn send(&self, mut payload: Encoded, exchange: Exchange) -> Opening<WireFrame> {
        match self.url.parse() {
            Ok(uri) => *payload.request.uri_mut() = uri,
            Err(error) => {
                return Opening::failed(ProviderError::request(format!(
                    "invalid endpoint URL: {error}"
                )));
            }
        }
        <DynHttpClient as Transport<W>>::send(&self.http, payload, exchange)
    }
}
