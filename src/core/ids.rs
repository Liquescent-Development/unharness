//! Identifiers for the three-level model: Harness → Provider → Model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A harness is the vendor CLI that runs the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessId {
    Agy,
    Claude,
    Codex,
    Pi,
}

impl HarnessId {
    /// Default resolution priority when no harness is requested or configured.
    pub const ALL: [HarnessId; 4] = [
        HarnessId::Agy,
        HarnessId::Claude,
        HarnessId::Codex,
        HarnessId::Pi,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "agy" | "antigravity" => Some(HarnessId::Agy),
            "claude" | "claude-code" => Some(HarnessId::Claude),
            "codex" => Some(HarnessId::Codex),
            "pi" => Some(HarnessId::Pi),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessId::Agy => "agy",
            HarnessId::Claude => "claude",
            HarnessId::Codex => "codex",
            HarnessId::Pi => "pi",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            HarnessId::Agy => "Antigravity (agy)",
            HarnessId::Claude => "Claude Code (claude)",
            HarnessId::Codex => "Codex (codex)",
            HarnessId::Pi => "pi",
        }
    }

    /// Short label for the TUI header.
    pub fn short_name(&self) -> &'static str {
        match self {
            HarnessId::Agy => "Antigravity",
            HarnessId::Claude => "Claude",
            HarnessId::Codex => "Codex",
            HarnessId::Pi => "pi",
        }
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for HarnessId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        HarnessId::parse(s).ok_or_else(|| {
            format!(
                "Unknown harness '{}'. Supported: {}",
                s,
                HarnessId::ALL
                    .iter()
                    .map(|h| h.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

/// A provider is the model vendor behind a harness (anthropic, openai, google, ollama, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(s: impl Into<String>) -> Self {
        ProviderId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ProviderId {
    fn from(s: &str) -> Self {
        ProviderId(s.to_string())
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fully qualified model selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub harness: HarnessId,
    pub provider: ProviderId,
    pub model: String,
}

impl ModelRef {
    pub fn new(
        harness: HarnessId,
        provider: impl Into<ProviderId>,
        model: impl Into<String>,
    ) -> Self {
        ModelRef {
            harness,
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// `provider/model`, used in the TUI header.
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.harness, self.provider, self.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert_eq!(HarnessId::parse("agy"), Some(HarnessId::Agy));
        assert_eq!(HarnessId::parse("Antigravity"), Some(HarnessId::Agy));
        assert_eq!(HarnessId::parse("claude-code"), Some(HarnessId::Claude));
        assert_eq!(HarnessId::parse("CODEX"), Some(HarnessId::Codex));
        assert_eq!(HarnessId::parse("pi"), Some(HarnessId::Pi));
        assert_eq!(HarnessId::parse("gemini"), None);
        assert!(
            "gemini"
                .parse::<HarnessId>()
                .unwrap_err()
                .contains("Supported")
        );
    }

    #[test]
    fn serde_roundtrip() {
        let m = ModelRef::new(HarnessId::Pi, "anthropic", "claude-sonnet-4-5");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"harness\":\"pi\""));
        assert!(json.contains("\"provider\":\"anthropic\""));
        let back: ModelRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(m.label(), "anthropic/claude-sonnet-4-5");
        assert_eq!(m.to_string(), "pi:anthropic/claude-sonnet-4-5");
    }
}
