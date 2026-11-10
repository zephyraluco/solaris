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
}
impl SolarisTransport {
    pub fn new(
        client: reqwest::Client,
        url: String,
        session_header: Option<&'static str>,
        session: &'static str,
    ) -> Self {
        Self {
            http: client.into(),
            url,
            session_header,
            session,
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
        <rig_reqwest::ReqwestClient as Transport<W>>::send(&self.http, payload, exchange)
    }
}
