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
    Gpt6Luna,
    Gpt6Sol,
    Gpt6dot1Sol,
    O4Mini,
}

impl Models {
    pub const USER_DEFAULT_MODEL: Self = Self::Gpt6Luna;
    pub const SYSTEM_MODEL: Self = Self::Gpt6Luna;
    pub const CRON_MODEL: Self = Self::Gpt6Luna;
    pub const CONTEXT_SUMMARY_MODEL: Self = Self::Gpt6Luna;

    pub fn list() -> Vec<Self> {
        vec![Self::Gpt6Luna]
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "gpt-6-luna" => Some(Self::Gpt6Luna),
            _ => None,
        }
    }

    pub fn rate_cost(&self) -> u64 {
        match self {
            Self::Gpt5dot4Mini | Self::Gpt5dot6Luna | Self::O4Mini => 3,
            Self::Gpt5dot4Nano => 1,
            Self::Gpt6Luna => 1,
            // Terra and the Sol models use the x20 rate-limit tier.
            Self::Gpt5dot6Terra | Self::Gpt6Sol | Self::Gpt6dot1Sol => 20,
        }
    }

    pub fn features(&self) -> ModelFeatures {
        ModelFeatures {
            prompt_cache_options: matches!(self, Self::Gpt5dot6Luna | Self::Gpt5dot6Terra),
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
            Self::Gpt6Luna => "gpt-6-luna",
            Self::Gpt6Sol => "gpt-6-sol",
            Self::Gpt6dot1Sol => "gpt-6.1-sol",
            Self::O4Mini => "o4-mini",
        };
        f.write_str(name)
    }
}

impl From<String> for Models {
    fn from(value: String) -> Self {
        Self::from_name(&value).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageGenerationModels {
    GptImage25Flare,
    GptImage25Sunburst,
}

impl ImageGenerationModels {
    pub const DEFAULT: Self = Self::GptImage25Flare;

    pub fn list() -> [Self; 2] {
        [Self::GptImage25Flare, Self::GptImage25Sunburst]
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "gpt-image-2.5-flare" => Some(Self::GptImage25Flare),
            "gpt-image-2.5-sunburst" => Some(Self::GptImage25Sunburst),
            _ => None,
        }
    }
}

impl Default for ImageGenerationModels {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Display for ImageGenerationModels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::GptImage25Flare => "gpt-image-2.5-flare",
            Self::GptImage25Sunburst => "gpt-image-2.5-sunburst",
        })
    }
}
