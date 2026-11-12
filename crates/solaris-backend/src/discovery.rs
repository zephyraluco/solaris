//! Official model-listing operations, with strict gateway envelope validation.
use rig_core::{
    driver::Model,
    error::{EncodeError, ProviderError},
    model::{ModelInfo, ModelList},
    operation::{ModelListing, ModelPage},
    wire::{
        Decoder, Descriptor, DocumentDecoder, Encoded, Flow, Mode, Out, Wire, WireEvent, WireFrame,
    },
};
use serde::Deserialize;

use crate::{Backend, BackendError};

impl Backend {
    pub(crate) async fn discover_models(&self) -> Result<Vec<String>, BackendError> {
        self.check_credentials()?;
        let http = self.request_http(&[]);
        let listing = async {
            if self.chatgpt.is_some() {
                Model::new(
                    CodexModels(rig_core::providers::openai::wire::Models::new(
                        self.openai_config(),
                    )),
                    http,
                )
                .list()
                .await
            } else if self.endpoint.wire == crate::Wire::AnthropicMessages {
                let wire = rig_core::providers::anthropic::modality::Models {
                    provider: self.anthropic_config(),
                };
                Model::new(StrictModels(wire), http).list().await
            } else {
                Model::new(
                    StrictModels(rig_core::providers::openai::wire::Models::new(
                        self.openai_config(),
                    )),
                    http,
                )
                .list()
                .await
            }
        };
        let models = tokio::time::timeout(std::time::Duration::from_secs(30), listing)
            .await
            .map_err(|_| BackendError::new("model discovery timed out"))?
            .map_err(|error| {
                let error = self.describe_error(&error.report());
                BackendError::new(format!("{error} ({})", self.endpoint.models_url()))
            })?;
        Ok(models.into_iter().map(|model| model.id).collect())
    }
}

// The official OpenAI envelope defaults a missing `data` to an empty list.
// A gateway's HTTP-200 error must remain an error, not a successful empty picker.
#[derive(Clone)]
struct StrictModels<W>(W);
enum Checked<E> {
    Native(E),
    Invalid(String),
}
struct StrictDecoder<D>(D);
impl<W: Wire<Op = ModelListing, Payload = Encoded, Frame = WireFrame>> Wire for StrictModels<W> {
    type Op = ModelListing;
    type Payload = Encoded;
    type Frame = WireFrame;
    type Event = Checked<W::Event>;
    type Decoder<'id> = StrictDecoder<W::Decoder<'id>>;
    type Reassembler = rig_core::wire::document::Unreassembled;
    fn describe(&self) -> Descriptor<'_> {
        self.0.describe()
    }
    fn encode(&self, cursor: Option<String>, mode: Mode) -> Result<Encoded, EncodeError> {
        self.0.encode(cursor, mode)
    }
    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        StrictDecoder(self.0.decoder())
    }
}
impl<'id, D: Decoder<'id, ModelListing>> Decoder<'id, ModelListing> for StrictDecoder<D> {
    type Event = Checked<D::Event>;
    fn classify(&self, frame: WireFrame) -> WireEvent<Self::Event> {
        let body = frame.as_str();
        if body.trim_start().starts_with('<') {
            return WireEvent::Known(Checked::Invalid(
                "answered with a web page, not a model list".into(),
            ));
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            && !value.get("data").is_some_and(serde_json::Value::is_array)
        {
            let detail =
                crate::http::provider_message(&body).unwrap_or_else(|| "missing data array".into());
            return WireEvent::Known(Checked::Invalid(format!(
                "did not answer with a model list: {detail}"
            )));
        }
        self.0.classify(frame).map(Checked::Native)
    }
    fn decode(
        &mut self,
        event: Self::Event,
        out: Out<'id, ModelListing>,
    ) -> Result<Flow, ProviderError> {
        match event {
            Checked::Native(event) => self.0.decode(event, out),
            Checked::Invalid(error) => Err(ProviderError::request(error)),
        }
    }
    fn eof(&mut self, out: Out<'id, ModelListing>) -> Result<Flow, ProviderError> {
        self.0.eof(out)
    }
}

// Rig has no subscription-specific Codex listing operation. Reuse its request
// encoding, transport, document decoder and listing driver; only map the schema.
#[derive(Clone)]
struct CodexModels(rig_core::providers::openai::wire::Models);
#[derive(Deserialize)]
struct CodexPage {
    models: Vec<CodexEntry>,
}
#[derive(Deserialize)]
struct CodexEntry {
    slug: String,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: i64,
}
impl Wire for CodexModels {
    type Op = ModelListing;
    type Payload = Encoded;
    type Frame = WireFrame;
    type Event = CodexPage;
    type Decoder<'id> = DocumentDecoder<ModelListing, CodexPage>;
    type Reassembler = rig_core::wire::document::Unreassembled;
    fn describe(&self) -> Descriptor<'_> {
        self.0.describe()
    }
    fn encode(&self, cursor: Option<String>, mode: Mode) -> Result<Encoded, EncodeError> {
        let mut encoded = self.0.encode(cursor, mode)?;
        // Codex gates the available catalog by its client protocol version.
        let url = format!("{}?client_version=0.162.1", encoded.request.uri());
        *encoded.request.uri_mut() = url
            .parse()
            .map_err(|error| EncodeError::request(format!("invalid catalog URL: {error}")))?;
        Ok(encoded)
    }
    #[expect(
        clippy::result_large_err,
        reason = "The SDK document decoder callback requires its native ProviderError."
    )]
    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        DocumentDecoder::new(
            |body| {
                rig_core::providers::internal::wire::classify_marker_keyed_frame(body, &["models"])
            },
            |mut page: CodexPage| {
                if page.models.iter().any(|model| model.slug.trim().is_empty()) {
                    return Err(ProviderError::request("model catalog contains an empty ID"));
                }
                page.models.sort_by_key(|model| model.priority);
                Ok(ModelPage {
                    models: ModelList::new(
                        page.models
                            .into_iter()
                            .filter(|model| {
                                model
                                    .visibility
                                    .as_deref()
                                    .is_none_or(|value| value == "list")
                            })
                            .map(|model| ModelInfo::from_id(model.slug))
                            .collect(),
                    ),
                    next: None,
                })
            },
        )
    }
}
