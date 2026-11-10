//! Protocol selection shared with provider configuration. Execution belongs to solaris-rig.

/// The wire protocol an endpoint speaks.
///
/// Three protocols cover every platform in the catalogue, which is why there is
/// one client for all of them. Choosing which one an endpoint speaks is the
/// caller's business; this crate only has to speak it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API, streamed from `POST /v1/messages`.
    AnthropicMessages,
    /// The OpenAI Chat Completions API, streamed from `POST /chat/completions`.
    ///
    /// The compatibility floor: every OpenAI-compatible upstream speaks this —
    /// New API, OpenRouter, a local Ollama or llama.cpp runtime, and Google's
    /// compatibility endpoint.
    OpenAiChat,
    /// The OpenAI Responses API, streamed from `POST /responses`.
    ///
    /// OpenAI's current first choice for new work, and what its newer models
    /// are documented against. Not as widely implemented by compatible
    /// upstreams as [`Wire::OpenAiChat`], so it is chosen per platform rather
    /// than assumed.
    OpenAiResponses,
}

impl Wire {
    /// Whether this is one of the OpenAI protocols.
    ///
    /// They share a host, a key and a caching story, which is what makes some
    /// request fields worth sending to both or to neither.
    pub fn is_openai(self) -> bool {
        matches!(self, Wire::OpenAiChat | Wire::OpenAiResponses)
    }
}
