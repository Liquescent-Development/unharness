//! Identifiers for the three-level model: Harness → Provider → Model.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A harness is the CLI that runs the agent loop: one of the built-in
/// adapters, or one defined in config (e.g. an ACP agent). Ids are interned
/// lowercase names, so the type stays `Copy` and compares by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HarnessId(&'static str);

impl HarnessId {
    pub const AGY: HarnessId = HarnessId("agy");
    pub const CLAUDE: HarnessId = HarnessId("claude");
    pub const CODEX: HarnessId = HarnessId("codex");
    pub const PI: HarnessId = HarnessId("pi");

    /// The harnesses with a dedicated adapter, in default resolution order.
    pub const BUILTIN: [HarnessId; 4] = [
        HarnessId::AGY,
        HarnessId::CLAUDE,
        HarnessId::CODEX,
        HarnessId::PI,
    ];

    /// A built-in harness by name or alias. Config-defined harnesses are
    /// resolved through the registry.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "agy" | "antigravity" => Some(HarnessId::AGY),
            "claude" | "claude-code" => Some(HarnessId::CLAUDE),
            "codex" => Some(HarnessId::CODEX),
            "pi" => Some(HarnessId::PI),
            _ => None,
        }
    }

    /// The id for `name`, built-in or not. Names are few and live for the
    /// whole run, so new ones are leaked once and reused.
    pub fn intern(name: &str) -> Self {
        let name = name.trim().to_lowercase();
        if let Some(builtin) = HarnessId::BUILTIN.iter().find(|b| b.0 == name) {
            return *builtin;
        }
        static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = names.get(name.as_str()) {
            return HarnessId(existing);
        }
        let leaked: &'static str = Box::leak(name.into_boxed_str());
        names.insert(leaked);
        HarnessId(leaked)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for HarnessId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;
        impl serde::de::Visitor<'_> for IdVisitor {
            type Value = HarnessId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a harness name")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<HarnessId, E> {
                Ok(HarnessId::intern(v))
            }
        }
        deserializer.deserialize_str(IdVisitor)
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

/// A model a harness offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub model_ref: ModelRef,
    pub display_name: String,
    pub description: Option<String>,
    /// Per-model effort levels when the harness reports them.
    pub effort_levels: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert_eq!(HarnessId::parse("agy"), Some(HarnessId::AGY));
        assert_eq!(HarnessId::parse("Antigravity"), Some(HarnessId::AGY));
        assert_eq!(HarnessId::parse("claude-code"), Some(HarnessId::CLAUDE));
        assert_eq!(HarnessId::parse("CODEX"), Some(HarnessId::CODEX));
        assert_eq!(HarnessId::parse("pi"), Some(HarnessId::PI));
        assert_eq!(HarnessId::parse("gemini"), None);
    }

    #[test]
    fn interned_ids_compare_by_name() {
        assert_eq!(HarnessId::intern("Claude"), HarnessId::CLAUDE);
        let a = HarnessId::intern("gemini");
        let b = HarnessId::intern(" GEMINI ");
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "gemini");
        assert_ne!(a, HarnessId::intern("opencode"));
        // A saved conversation naming a harness this build lacks still loads.
        let back: HarnessId = serde_json::from_str("\"gemini\"").unwrap();
        assert_eq!(back, a);
        let map: std::collections::HashMap<HarnessId, u8> =
            serde_json::from_str(r#"{"gemini":1,"pi":2}"#).unwrap();
        assert_eq!(map[&a], 1);
        assert_eq!(map[&HarnessId::PI], 2);
        assert_eq!(serde_json::to_string(&HarnessId::PI).unwrap(), "\"pi\"");
    }

    #[test]
    fn serde_roundtrip() {
        let m = ModelRef::new(HarnessId::PI, "anthropic", "claude-sonnet-4-5");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"harness\":\"pi\""));
        assert!(json.contains("\"provider\":\"anthropic\""));
        let back: ModelRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(m.label(), "anthropic/claude-sonnet-4-5");
        assert_eq!(m.to_string(), "pi:anthropic/claude-sonnet-4-5");
    }
}
