use std::fmt::{self, Display};

use async_openai::types::responses::ReasoningEffort;

#[derive(Debug, Clone)]
pub struct ModelResponseParams {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub features: ModelFeatures,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFeatures {
    pub prompt_cache_options: bool,
    pub parallel_tool_calls: bool,
    pub cached_tokens: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Models {
    Gpt5dot4Mini,
    Gpt5dot4Nano,
    Gpt5dot6Luna,
    Gpt5dot6Terra,
    Gpt5dot6Sol,
    O4Mini,
    O3,
}

impl Models {
    pub const USER_DEFAULT_MODEL: Self = Self::Gpt5dot6Luna;
    pub const SYSTEM_MODEL: Self = Self::Gpt5dot6Luna;
    pub const CRON_MODEL: Self = Self::Gpt5dot6Luna;
    pub const CONTEXT_SUMMARY_MODEL: Self = Self::Gpt5dot6Luna;

    pub fn list() -> Vec<Self> {
        vec![
            Self::Gpt5dot4Mini,
            Self::Gpt5dot4Nano,
            Self::Gpt5dot6Luna,
            Self::Gpt5dot6Terra,
            Self::Gpt5dot6Sol,
            Self::O4Mini,
            Self::O3,
        ]
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "gpt-5.4-mini" => Some(Self::Gpt5dot4Mini),
            "gpt-5.4-nano" => Some(Self::Gpt5dot4Nano),
            "gpt-5.6-luna" => Some(Self::Gpt5dot6Luna),
            "gpt-5.6-terra" => Some(Self::Gpt5dot6Terra),
            "gpt-5.6-sol" => Some(Self::Gpt5dot6Sol),
            "o4-mini" => Some(Self::O4Mini),
            "o3" => Some(Self::O3),
            _ => None,
        }
    }

    pub fn rate_cost(&self) -> u64 {
        match self {
            Self::Gpt5dot4Mini | Self::Gpt5dot6Luna | Self::O4Mini => 3,
            Self::Gpt5dot4Nano => 1,
            Self::Gpt5dot6Terra | Self::O3 => 6,
            Self::Gpt5dot6Sol => 9,
        }
    }

    pub fn features(&self) -> ModelFeatures {
        ModelFeatures {
            prompt_cache_options: matches!(
                self,
                Self::Gpt5dot6Luna | Self::Gpt5dot6Terra | Self::Gpt5dot6Sol
            ),
            parallel_tool_calls: true,
            cached_tokens: true,
        }
    }

    pub fn to_parameter(&self) -> ModelResponseParams {
        ModelResponseParams {
            model: self.to_string(),
            reasoning_effort: ReasoningEffort::Low,
            features: self.features(),
        }
    }
}

impl Default for Models {
    fn default() -> Self {
        Self::USER_DEFAULT_MODEL
    }
}

impl Display for Models {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Gpt5dot4Mini => "gpt-5.4-mini",
            Self::Gpt5dot4Nano => "gpt-5.4-nano",
            Self::Gpt5dot6Luna => "gpt-5.6-luna",
            Self::Gpt5dot6Terra => "gpt-5.6-terra",
            Self::Gpt5dot6Sol => "gpt-5.6-sol",
            Self::O4Mini => "o4-mini",
            Self::O3 => "o3",
        };
        f.write_str(name)
    }
}

impl From<String> for Models {
    fn from(value: String) -> Self {
        Self::from_name(&value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purpose_models_are_explicit_and_user_default_drives_default() {
        assert_eq!(Models::default(), Models::USER_DEFAULT_MODEL);
        assert_eq!(Models::USER_DEFAULT_MODEL, Models::Gpt5dot6Luna);
        assert_eq!(Models::SYSTEM_MODEL, Models::Gpt5dot6Luna);
        assert_eq!(Models::CRON_MODEL, Models::Gpt5dot6Luna);
        assert_eq!(Models::CONTEXT_SUMMARY_MODEL, Models::Gpt5dot6Luna);
    }

    #[test]
    fn gpt_5_6_models_have_expected_names_and_costs() {
        let cases = [
            (Models::Gpt5dot6Luna, "gpt-5.6-luna", 3),
            (Models::Gpt5dot6Terra, "gpt-5.6-terra", 6),
            (Models::Gpt5dot6Sol, "gpt-5.6-sol", 9),
        ];

        for (model, name, cost) in cases {
            assert_eq!(model.to_string(), name);
            assert_eq!(model.rate_cost(), cost);
            assert_eq!(Models::from(name.to_string()).to_string(), name);
        }
    }

    #[test]
    fn listed_models_support_parallel_tools_and_cached_token_usage() {
        for model in Models::list() {
            let features = model.features();
            assert!(features.parallel_tool_calls, "{model}");
            assert!(features.cached_tokens, "{model}");
        }
    }

    #[test]
    fn only_gpt_5_6_models_support_prompt_cache_options() {
        for model in Models::list() {
            let expected = matches!(
                model,
                Models::Gpt5dot6Luna | Models::Gpt5dot6Terra | Models::Gpt5dot6Sol
            );
            assert_eq!(model.features().prompt_cache_options, expected, "{model}");
            assert_eq!(model.to_parameter().features, model.features(), "{model}");
        }
    }
}
