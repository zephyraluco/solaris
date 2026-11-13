//! One immutable model parameter snapshot derived from official facts.
use rig_core::catalog::{Catalog, ModelFacts, ModelSpec};
use serde::Serialize;
use solaris_types::{ModelCapabilities, Support};
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;
pub const DEFAULT_MAX_OUTPUT: u32 = 8192;
#[derive(Debug, Clone, Serialize)]
pub struct ModelParameters {
    #[serde(serialize_with = "serialize_facts")]
    facts: ModelFacts,
    capabilities: ModelCapabilities,
    output_tokens: u32,
    #[serde(skip)]
    known: bool,
}
fn serialize_facts<S: serde::Serializer>(
    facts: &ModelFacts,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    facts.spec().serialize(serializer)
}
impl ModelParameters {
    pub fn new(spec: ModelSpec, requested_output: Option<u32>, known: bool) -> Self {
        let mut capabilities = capabilities(&spec, known);
        let context = spec.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let requested = requested_output.unwrap_or(DEFAULT_MAX_OUTPUT.min(context / 4).max(1));
        let output_tokens = requested
            .min(spec.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT))
            .min(context.saturating_sub(1).max(1))
            .max(1);
        capabilities.output_budget = Some(output_tokens);
        Self {
            facts: ModelFacts::new(spec).with_catalog(&Catalog::default()),
            capabilities,
            output_tokens,
            known,
        }
    }
    pub fn facts(&self) -> &ModelFacts {
        &self.facts
    }
    pub fn model(&self) -> &str {
        &self.facts.spec().expect("bound facts").id
    }
    pub fn capabilities(&self) -> ModelCapabilities {
        self.capabilities.clone()
    }
    pub fn output_tokens(&self) -> u32 {
        self.output_tokens
    }
    pub fn without_pricing(self) -> Self {
        let mut spec = self.facts.spec().expect("bound facts").clone();
        spec.pricing = None;
        Self::new(spec, Some(self.output_tokens), self.known)
    }
}
fn capabilities(spec: &ModelSpec, known: bool) -> ModelCapabilities {
    fn support(value: bool, known: bool) -> Support {
        if !known {
            Support::Unknown
        } else if value {
            Support::Supported
        } else {
            Support::Unsupported
        }
    }
    ModelCapabilities {
        tool_calling: support(spec.tools, known),
        structured_output: support(spec.structured_output, known),
        image_input: support(spec.input.image, known),
        reasoning: support(spec.reasoning.supported(), known),
        prompt_caching: if spec.caching.retention.is_empty() {
            Support::Unknown
        } else {
            Support::Supported
        },
        temperature: match spec.sampling {
            Some(rig_core::catalog::Sampling::Any) => Support::Supported,
            Some(rig_core::catalog::Sampling::Never) => Support::Unsupported,
            _ => Support::Unknown,
        },
        context_window: spec.context_window,
        max_input: spec.max_input_tokens,
        max_output: spec.max_output_tokens,
        price_known: Some(spec.pricing.is_some()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> ModelSpec {
        ModelSpec::new(
            rig_core::providers::registry::ProviderId::declare("test").unwrap(),
            "alias",
        )
    }
    #[test]
    fn requested_output_is_bounded_by_the_same_facts_used_for_capabilities() {
        let parameters = ModelParameters::new(
            spec()
                .with_context_window(8192)
                .with_max_output_tokens(4096),
            Some(10000),
            true,
        );
        assert_eq!(parameters.output_tokens(), 4096);
        assert_eq!(parameters.capabilities().output_budget, Some(4096));
        assert_eq!(parameters.capabilities().context_window, Some(8192));
        assert_eq!(parameters.capabilities().tool_calling, Support::Unsupported);
        assert_eq!(parameters.facts().spec().unwrap().id, parameters.model());
    }
    #[test]
    fn removing_subscription_pricing_keeps_the_other_parameters() {
        let mut spec = spec().with_tools(true).with_context_window(32000);
        spec.pricing = Some(rig_core::catalog::Pricing::new(1.0, 2.0));
        let original = ModelParameters::new(spec, Some(2048), true);
        assert_eq!(original.capabilities().price_known, Some(true));
        let account = original.without_pricing();
        assert_eq!(account.capabilities().price_known, Some(false));
        assert!(account.facts().spec().unwrap().pricing.is_none());
        assert_eq!(account.output_tokens(), 2048);
        assert_eq!(account.capabilities().tool_calling, Support::Supported);
    }
}
