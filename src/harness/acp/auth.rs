//! Sign-in: the `authMethods` an agent offers at `initialize`, and the
//! spec's auth-required error (-32000) it answers a session request or a
//! prompt with when the user is not signed in.
//!
//! unharness never sends `authenticate` and runs no sign-in itself: it
//! tells the user what the agent offers and, where the harness knows it,
//! what to run (`[harnesses.<name>] sign_in`, or a harness that declares
//! its own). A terminal sign-in would run a command the agent names,
//! outside the sandbox and in the user's terminal; a CLI's own login is
//! left to the user for every other harness, and so is this one. That is
//! also why the client capabilities declare no terminal auth (neither the
//! spec's `auth.terminal` nor vibe-acp's `_meta["terminal-auth"]`).

use serde_json::Value;

use super::parse::error_text;

/// The spec's error for a request the user must be signed in for.
pub const AUTH_REQUIRED: i64 = -32000;

/// Whether a JSON-RPC error is the spec's auth-required one.
pub fn is_auth_required(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(AUTH_REQUIRED)
}

/// One of the agent's `authMethods`, as far as it is shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMethod {
    pub name: String,
    pub description: Option<String>,
    /// An `env_var` method's variables, by name.
    pub vars: Vec<String>,
}

/// The `authMethods` of an `initialize` result. A method without a name
/// goes by its id; one with neither is left out.
pub fn auth_methods(init_result: &Value) -> Vec<AuthMethod> {
    let Some(methods) = init_result.get("authMethods").and_then(Value::as_array) else {
        return Vec::new();
    };
    methods
        .iter()
        .filter_map(|m| {
            let text = |key: &str| {
                m.get(key)
                    .and_then(Value::as_str)
                    .map(one_line)
                    .filter(|s| !s.is_empty())
            };
            let name = text("name").or_else(|| text("id"))?;
            let vars = match m.get("type").and_then(Value::as_str) {
                Some("env_var") => m
                    .get("vars")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.get("name").and_then(Value::as_str).map(one_line))
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => Vec::new(),
            };
            Some(AuthMethod {
                name,
                description: text("description"),
                vars,
            })
        })
        .collect()
}

/// What the user is told when `agent` answered with the auth-required
/// `error`: the methods it offers and, when the harness knows it, the
/// command that signs it in (`sign_in`).
pub fn not_signed_in(
    agent: &str,
    error: &Value,
    methods: &[AuthMethod],
    sign_in: Option<&str>,
) -> String {
    let why = error_text(error);
    let mut text = format!(
        "{agent} is not signed in ({})",
        why.trim().trim_end_matches('.')
    );
    if !methods.is_empty() {
        let offered: Vec<String> = methods
            .iter()
            .map(|m| {
                let mut s = format!("\"{}\"", m.name);
                if let Some(d) = &m.description {
                    s.push_str(&format!(" ({})", d.trim_end_matches('.')));
                }
                if !m.vars.is_empty() {
                    s.push_str(&format!(", set {}", m.vars.join(", ")));
                }
                s
            })
            .collect();
        text.push_str(&format!(". It offers: {}", offered.join("; ")));
    }
    match sign_in {
        Some(command) => text.push_str(&format!(
            ". unharness does not sign agents in: run `{command}` in a terminal, then try again"
        )),
        None => text.push_str(&format!(
            ". unharness does not sign agents in: sign in with {agent}'s own command line, then try again"
        )),
    }
    text
}

/// Agent text on one line, its runs of whitespace one space.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::HarnessId;
    use crate::core::testing::{assert_fixture, fixtures_dir};
    use crate::harness::acp::parse::AcpParser;
    use serde_json::json;

    /// vibe-acp 2.26.1's answer without terminal auth declared
    /// (`fixtures/vibe_acp_not_signed_in.jsonl`).
    fn vibe_init() -> Value {
        json!({"protocolVersion": 1, "authMethods": [{
            "id": "browser-auth",
            "name": "Sign in through Mistral AI Studio",
            "description": "Sign into Mistral Vibe through your Mistral AI Studio account."
        }]})
    }

    fn vibe_error() -> Value {
        json!({"code": -32000, "message": "Missing API key for mistral provider.", "data": null})
    }

    #[test]
    fn the_auth_required_error_is_the_specs_code() {
        assert!(is_auth_required(&vibe_error()));
        // vibe-acp 2.26.1's answer to a prompt with a key it does not take.
        assert!(!is_auth_required(&json!({
            "code": -31002,
            "message": "Invalid API key (from env var MISTRAL_API_KEY). Please check your API key and try again."
        })));
        assert!(!is_auth_required(&json!({"message": "x"})));
    }

    #[test]
    fn the_message_names_the_methods_and_what_to_run() {
        let methods = auth_methods(&vibe_init());
        assert_eq!(
            not_signed_in("vibe", &vibe_error(), &methods, Some("vibe --setup")),
            "vibe is not signed in (Missing API key for mistral provider). It offers: \
             \"Sign in through Mistral AI Studio\" (Sign into Mistral Vibe through your \
             Mistral AI Studio account). unharness does not sign agents in: run \
             `vibe --setup` in a terminal, then try again"
        );
        // Without a command known, the agent's own CLI.
        let text = not_signed_in("my-agent", &vibe_error(), &[], None);
        assert_eq!(
            text,
            "my-agent is not signed in (Missing API key for mistral provider). unharness \
             does not sign agents in: sign in with my-agent's own command line, then try again"
        );
    }

    /// The recorded lines of a fixture that parse as JSON.
    fn recorded(case: &str) -> Vec<Value> {
        let path = fixtures_dir(file!()).join(format!("{case}.jsonl"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// vibe-acp 2.26.1 with no key: `session/new` is answered with the
    /// spec's auth-required error, which the parser shows as it is (the
    /// driver words it, `transport.rs`).
    #[test]
    fn fixture_vibe_acp_not_signed_in() {
        let dir = fixtures_dir(file!());
        let mut parser = AcpParser::new(HarnessId::intern("acp-test"));
        assert_fixture(&mut parser, &dir, "vibe_acp_not_signed_in");
        let lines = recorded("vibe_acp_not_signed_in");
        let init = lines.iter().find_map(|v| v.get("result")).unwrap();
        assert_eq!(auth_methods(init), auth_methods(&vibe_init()));
        let error = lines.iter().find_map(|v| v.get("error")).unwrap();
        assert_eq!(error, &vibe_error());
        assert!(is_auth_required(error));
    }

    /// With vibe-acp's `_meta["terminal-auth"]` declared (unharness does
    /// not) it also offers `vibe-setup`, whose command is its own Python
    /// running the `vibe-acp` script: shown by name, never as a command.
    #[test]
    fn fixture_vibe_acp_terminal_auth() {
        let dir = fixtures_dir(file!());
        let mut parser = AcpParser::new(HarnessId::intern("acp-test"));
        assert_fixture(&mut parser, &dir, "vibe_acp_terminal_auth");
        let lines = recorded("vibe_acp_terminal_auth");
        let init = lines.iter().find_map(|v| v.get("result")).unwrap();
        let methods = auth_methods(init);
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[1].name, "Register your API Key");
        let text = not_signed_in("vibe", &vibe_error(), &methods, None);
        assert!(
            !text.contains("--setup") && !text.contains("python"),
            "{text}"
        );
    }

    #[test]
    fn methods_of_every_type_are_read() {
        let init = json!({"authMethods": [
            {"id": "a", "name": "Agent\nsign-in", "description": "  "},
            {"id": "env", "name": "Key", "type": "env_var",
             "vars": [{"name": "API_KEY"}, {"name": "ORG"}], "link": "https://example.com"},
            {"id": "t", "type": "terminal", "args": ["--setup"]},
            {"description": "no name, no id"},
        ]});
        let methods = auth_methods(&init);
        assert_eq!(
            methods,
            [
                AuthMethod {
                    name: "Agent sign-in".into(),
                    description: None,
                    vars: vec![],
                },
                AuthMethod {
                    name: "Key".into(),
                    description: None,
                    vars: vec!["API_KEY".into(), "ORG".into()],
                },
                AuthMethod {
                    name: "t".into(),
                    description: None,
                    vars: vec![],
                },
            ]
        );
        let text = not_signed_in("x", &json!({"code": -32000}), &methods, None);
        assert!(
            text.contains(r#"It offers: "Agent sign-in"; "Key", set API_KEY, ORG; "t". "#),
            "{text}"
        );
        assert!(auth_methods(&json!({})).is_empty());
    }
}
