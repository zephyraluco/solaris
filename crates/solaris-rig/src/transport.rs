//! Preserve configured gateway paths and session headers while Rig handles HTTP framing.
use rig_core::{
    driver::{Exchange, Opening, Transport},
    error::ProviderError,
    wire::{Encoded, Wire, WireFrame},
};

#[derive(Clone)]
pub(crate) struct SolarisTransport {
    http: rig_reqwest::ReqwestClient,
    url: String,
    session_header: Option<&'static str>,
    session: &'static str,
    capabilities: solaris_backend::ModelCapabilities,
    repairs: Vec<crate::policy::Repair>,
}
impl SolarisTransport {
    pub fn new(
        client: reqwest::Client,
        url: String,
        session_header: Option<&'static str>,
        session: &'static str,
        capabilities: solaris_backend::ModelCapabilities,
        repairs: Vec<crate::policy::Repair>,
    ) -> Self {
        Self {
            http: client.into(),
            url,
            session_header,
            session,
            capabilities,
            repairs,
        }
    }
}
impl<W: Wire<Payload = Encoded, Frame = WireFrame>> Transport<W> for SolarisTransport {
    fn send(&self, mut payload: Encoded, exchange: Exchange) -> Opening<WireFrame> {
        let uri = match self.url.parse() {
            Ok(uri) => uri,
            Err(error) => {
                return Opening::failed(ProviderError::request(format!(
                    "invalid endpoint URL: {error}"
                )));
            }
        };
        *payload.request.uri_mut() = uri;
        if let Some(name) = self.session_header {
            let name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                Ok(name) => name,
                Err(error) => return Opening::failed(ProviderError::request(error.to_string())),
            };
            payload.request.headers_mut().insert(
                name,
                reqwest::header::HeaderValue::from_static(self.session),
            );
        }
        use solaris_backend::Support;
        let enforce_policy = !self.repairs.is_empty()
            || self.capabilities.streaming_usage == Support::Unsupported
            || self.capabilities.prompt_caching == Support::Unsupported
            || self.capabilities.structured_output == Support::Supported;
        if enforce_policy && let rig_core::wire::Body::Bytes(bytes) = payload.request.body_mut() {
            let mut body: serde_json::Value = match serde_json::from_slice(bytes) {
                Ok(value) => value,
                Err(error) => return Opening::failed(ProviderError::request(error.to_string())),
            };
            crate::policy::apply_capabilities(&mut body, &self.capabilities);
            for repair in &self.repairs {
                crate::policy::apply_repair(&mut body, *repair);
            }
            *bytes = match serde_json::to_vec(&body) {
                Ok(bytes) => bytes,
                Err(error) => return Opening::failed(ProviderError::request(error.to_string())),
            };
        }
        <rig_reqwest::ReqwestClient as Transport<W>>::send(&self.http, payload, exchange)
    }
}
