//! The providers the app can talk to.
//!
//! Everything here except Anthropic speaks OpenAI's Chat Completions wire, so a
//! preset is only an endpoint, the environment variable its key lives in, and
//! whatever model names it can offer without asking the provider. DeepSeek is the
//! one the app defaults to; the rest exist so a person is not locked into it:
//! `custom` is any OpenAI-compatible endpoint, which covers most of the field
//! (vLLM, LM Studio, a company gateway) and `ollama` is the common local one.

use crate::ModelInfo;

/// Which wire a provider speaks. Everything except Anthropic uses OpenAI's Chat
/// Completions shape; Anthropic's Messages API is its own adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    OpenAiCompatible,
    Anthropic,
}

/// How a provider spells "think harder" on its wire. The app has one thinking
/// switch; each preset says whether its API has a field for it, and which one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reasoning {
    /// No switch. Anthropic is here on purpose: its `thinking` only works when
    /// the assistant's thinking blocks are sent back, and the transcript never
    /// keeps them, so asking would break the next tool round.
    None,
    /// DeepSeek: `thinking: {type: enabled|disabled}` plus
    /// `reasoning_effort: low|high|max`.
    Thinking,
    /// OpenAI and OpenRouter: `reasoning_effort: low|medium|high`.
    Effort,
}

/// One provider, as the settings window and the resolver see it.
pub struct Preset {
    /// What `[provider] id` names, and what an error message calls the provider.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Where requests go. Empty when the person has to supply one.
    pub base_url: &'static str,
    /// The environment variable the key is read from. `None` for an endpoint that
    /// needs no key; `[provider] key_env` adds one.
    pub key_env: Option<&'static str>,
    /// The models the picker can offer without asking the provider. Empty when
    /// only the provider itself knows, which the settings window refreshes.
    pub models: &'static [ModelInfo],
    /// The adapter that talks to it.
    pub wire: Wire,
    /// Whether, and how, the wire carries a thinking switch.
    pub reasoning: Reasoning,
}

impl Preset {
    /// The model to use when the profile names none. A provider with no catalogue
    /// has none, and the resolver says so rather than sending a guess.
    pub fn default_model(&self) -> Option<&'static str> {
        self.models.first().map(|model| model.id)
    }

    /// What this provider's catalogue says about a model. The same id can be a
    /// different model at another provider, so this is the only lookup that can
    /// answer what a model is worth here.
    pub fn model(&self, id: &str) -> Option<&'static ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }
}

pub const DEEPSEEK_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "deepseek-flash",
        display_name: "DeepSeek V4.1 Flash",
        context_tokens: 1_000_000,
    },
    ModelInfo {
        id: "deepseek-v4-pro",
        display_name: "DeepSeek V4 Pro",
        context_tokens: 1_000_000,
    },
];

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "deepseek",
        display_name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        key_env: Some("DEEPSEEK_API_KEY"),
        models: DEEPSEEK_MODELS,
        wire: Wire::OpenAiCompatible,
        reasoning: Reasoning::Thinking,
    },
    Preset {
        id: "openai",
        display_name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        key_env: Some("OPENAI_API_KEY"),
        models: &[],
        wire: Wire::OpenAiCompatible,
        reasoning: Reasoning::Effort,
    },
    Preset {
        id: "openrouter",
        display_name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
        models: &[],
        wire: Wire::OpenAiCompatible,
        reasoning: Reasoning::Effort,
    },
    Preset {
        id: "anthropic",
        display_name: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
        key_env: Some("ANTHROPIC_API_KEY"),
        models: &[],
        wire: Wire::Anthropic,
        reasoning: Reasoning::None,
    },
    Preset {
        id: "ollama",
        display_name: "Ollama",
        base_url: "http://localhost:11434/v1",
        key_env: None,
        models: &[],
        wire: Wire::OpenAiCompatible,
        reasoning: Reasoning::None,
    },
    Preset {
        id: "custom",
        display_name: "Custom endpoint",
        base_url: "",
        key_env: None,
        models: &[],
        wire: Wire::OpenAiCompatible,
        reasoning: Reasoning::None,
    },
];

/// The provider a profile talks to when it names none.
pub const DEFAULT_PROVIDER: &str = "deepseek";

/// The model a fresh profile starts with.
pub const DEFAULT_MODEL: &str = DEEPSEEK_MODELS[0].id;

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// Every model the app knows without asking a provider, in preset order.
pub fn models() -> impl Iterator<Item = &'static ModelInfo> {
    PRESETS.iter().flat_map(|preset| preset.models.iter())
}

/// The models every preset catalogs, for labels and for a picker that does not know
/// which provider it is looking at. What a model is *worth* — its context window —
/// comes from the provider itself ([`Provider::context_limit`]), never from here.
pub fn model_info(id: &str) -> Option<&'static ModelInfo> {
    models().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_says_what_it_is_and_how_to_reach_it() {
        for (index, preset) in PRESETS.iter().enumerate() {
            assert!(!preset.id.is_empty());
            assert!(!preset.display_name.is_empty(), "{}", preset.id);
            assert!(
                !PRESETS[..index].iter().any(|seen| seen.id == preset.id),
                "{} is listed twice",
                preset.id
            );
            // `custom` is the one provider whose endpoint only the person knows.
            if preset.id == "custom" {
                assert!(preset.base_url.is_empty());
            } else {
                assert!(
                    preset.base_url.starts_with("http"),
                    "{} has no endpoint",
                    preset.id
                );
            }
            if let Some(model) = preset.default_model() {
                assert!(
                    preset.models.iter().any(|info| info.id == model),
                    "{} defaults to a model it does not list",
                    preset.id
                );
            }
        }
        assert_eq!(
            preset(DEFAULT_PROVIDER).map(|preset| preset.id),
            Some("deepseek"),
            "the default provider has to exist"
        );
    }

    #[test]
    fn every_listed_model_is_found_by_id() {
        for model in models() {
            assert!(model.context_tokens > 0, "{}", model.id);
            assert_eq!(model_info(model.id).map(|info| info.id), Some(model.id));
        }
        assert!(model_info("no-such-model").is_none());
    }

    #[test]
    fn a_provider_that_needs_no_key_is_the_one_without_a_variable() {
        let keyed: Vec<_> = PRESETS
            .iter()
            .filter(|preset| preset.key_env.is_some())
            .map(|preset| preset.id)
            .collect();
        let keyless: Vec<_> = PRESETS
            .iter()
            .filter(|preset| preset.key_env.is_none())
            .map(|preset| preset.id)
            .collect();

        assert_eq!(keyed, ["deepseek", "openai", "openrouter", "anthropic"]);
        assert_eq!(keyless, ["ollama", "custom"]);
    }

    #[test]
    fn only_anthropic_speaks_its_own_wire() {
        let anthropic: Vec<_> = PRESETS
            .iter()
            .filter(|preset| preset.wire == Wire::Anthropic)
            .map(|preset| preset.id)
            .collect();

        assert_eq!(anthropic, ["anthropic"]);
    }
}
