//! An agent's own session modes, held to the permission policy.
//!
//! An ACP agent may approve calls by itself in one of its modes: Mistral
//! Vibe's `vibe-acp` 2.26.1 starts every session in `accept-edits`, which
//! writes files without a `session/request_permission`, and a loaded
//! session comes back in the mode it was left in (`auto-approve` in
//! `fixtures/vibe_acp_mode_load.jsonl`). The policy is applied by unharness
//! to the requests an agent sends (`transport::auto_allows`), so what it
//! runs without asking has to be ruled out by its mode: a [`ModeMap`] says
//! which of the agent's modes backs each policy, the transport sets that
//! mode before the first prompt and after every change, reads it back, and
//! holds the session when it did not take ([`mode_step`], [`read_back`]).
//!
//! Which mode asks is the agent's own business, not the protocol's, so the
//! map is declared by a harness that knows its agent, or set in
//! `[harnesses.<name>] modes` for one defined in the config.

use std::collections::BTreeMap;

use serde_json::Value;

use super::parse::SessionOptions;
use crate::core::PermissionPolicy;

/// Which of an agent's session modes backs each unharness policy.
///
/// A policy without an entry takes the entry of the nearest less
/// permissive policy that has one: with `ask = "ask"` alone the agent stays
/// in its asking mode under every policy and unharness answers what the
/// policy allows by itself. Never the entry of a more permissive one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeMap(BTreeMap<PermissionPolicy, String>);

impl ModeMap {
    pub fn new<S: Into<String>>(entries: impl IntoIterator<Item = (PermissionPolicy, S)>) -> Self {
        ModeMap(entries.into_iter().map(|(p, m)| (p, m.into())).collect())
    }

    /// From a `[harnesses.<name>] modes` table. Every key must be a policy
    /// and every value a mode id; a table that is not is an error, never
    /// an empty map (which would leave the agent in its own mode).
    pub fn from_config(table: &BTreeMap<String, String>) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for (policy, mode) in table {
            let p = PermissionPolicy::parse(policy).ok_or_else(|| {
                format!(
                    "unknown policy '{policy}' in modes (plan, ask, accept-edits, auto, bypass)"
                )
            })?;
            if mode.trim().is_empty() {
                return Err(format!("modes.{policy} names no mode"));
            }
            map.insert(p, mode.clone());
        }
        Ok(ModeMap(map))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the map names a mode for `policy` itself, rather than
    /// leaving it to a less permissive policy's entry.
    pub fn names(&self, policy: PermissionPolicy) -> bool {
        self.0.contains_key(&policy)
    }

    /// The mode for `policy`: its own entry, else the nearest less
    /// permissive policy's, with the policy whose entry it is.
    pub fn for_policy(&self, policy: PermissionPolicy) -> Option<(PermissionPolicy, &str)> {
        self.0
            .range(..=policy)
            .next_back()
            .map(|(p, m)| (*p, m.as_str()))
    }

    /// Whether being in `mode` widens nothing under `policy`: it is the
    /// mode of `policy` or of a less permissive one.
    pub fn within(&self, policy: PermissionPolicy, mode: &str) -> bool {
        self.0.range(..=policy).any(|(_, m)| m == mode)
    }
}

/// What the session said about its modes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentModes {
    /// The session answered with its modes, or is a new one (which lists
    /// them if it has any). `false` after a reattach whose answer named
    /// none: whether it has modes, and which it is in, is not known.
    pub known: bool,
    pub current: Option<String>,
    /// `modes.availableModes`, for `session/set_mode`.
    pub listed: Vec<String>,
    /// The select config option of category `mode`, and its values.
    pub option: Option<String>,
    pub option_values: Vec<String>,
}

impl AgentModes {
    /// From a session answer (`session/new`, `load`, `resume`, `fork`) and
    /// the options read from it.
    pub fn from_session(result: &Value, options: &SessionOptions, reattach: bool) -> Self {
        let modes = result.get("modes").filter(|m| m.is_object());
        let mut out = AgentModes {
            known: !reattach || modes.is_some() || options.mode_option.is_some(),
            current: modes
                .and_then(|m| m.get("currentModeId"))
                .and_then(Value::as_str)
                .map(str::to_string),
            listed: modes
                .and_then(|m| m.get("availableModes"))
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|m| m.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            ..Default::default()
        };
        out.take_options(options);
        out
    }

    /// The mode option as a config answer or update has it now.
    pub fn take_options(&mut self, options: &SessionOptions) {
        if let Some(id) = &options.mode_option {
            self.known = true;
            self.option = Some(id.clone());
            self.option_values = options.mode_values.clone();
            if let Some(current) = &options.current_mode {
                self.current = Some(current.clone());
            }
        }
    }

    /// The session has modes of its own.
    pub fn any(&self) -> bool {
        !self.listed.is_empty() || !self.option_values.is_empty()
    }

    /// Every mode id the session offers, in order, once.
    pub fn offered(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for m in self.listed.iter().chain(&self.option_values) {
            if !out.contains(&m.as_str()) {
                out.push(m);
            }
        }
        out
    }

    /// How `mode` is set: through the mode option when the session offers
    /// it with that value (its answer carries the option list, which reads
    /// the mode back), else `session/set_mode` when it lists the mode.
    /// `None` when it offers no such mode.
    fn channel(&self, mode: &str) -> Option<SetVia> {
        match &self.option {
            Some(id) if self.option_values.iter().any(|v| v == mode) => {
                Some(SetVia::Option(id.clone()))
            }
            _ => self
                .listed
                .iter()
                .any(|m| m == mode)
                .then_some(SetVia::SetMode),
        }
    }
}

/// The request that sets a mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetVia {
    /// `session/set_config_option` of this option id.
    Option(String),
    /// `session/set_mode`.
    SetMode,
}

/// What the transport does about the agent's mode for a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeStep {
    /// Nothing is mapped: the agent's mode is left as it is.
    Leave,
    /// The agent is in the mode already.
    Keep(String),
    /// Set this mode, and wait for the answer before a prompt goes.
    Set { mode: String, via: SetVia },
    /// The policy cannot be held in this session: why.
    Refuse(String),
}

/// What to do so that the agent of `harness` runs under `policy`.
///
/// Only `bypass` is left to the agent's own mode. An agent with modes and
/// no map has every policy below it refused, as a policy a harness cannot
/// hold is, and the message names the missing map: whatever mode the agent
/// is in may run what the policy asks about (a vibe session loaded in
/// `auto-approve` would run commands unasked under `accept-edits`). With a
/// map, every policy needs an entry at or below it.
pub fn mode_step(
    harness: &str,
    policy: PermissionPolicy,
    map: &ModeMap,
    agent: &AgentModes,
) -> ModeStep {
    let unavailable = |why: String| {
        ModeStep::Refuse(format!(
            "policy '{policy}' is not available for {harness}: {why}"
        ))
    };
    let Some((_, mode)) = map.for_policy(policy) else {
        if !agent.any() || (map.is_empty() && policy >= PermissionPolicy::Bypass) {
            return ModeStep::Leave;
        }
        return unavailable(if map.is_empty() {
            format!(
                "it has session modes of its own ({}), some of which run tools without asking, \
                 and [harnesses.{harness}] has no `modes` map to say which one holds to {policy} \
                 (e.g. modes = {{ ask = \"…\" }}; see docs/acp.md)",
                agent.offered().join(", ")
            )
        } else {
            format!(
                "[harnesses.{harness}] modes names no mode for {policy} or a less permissive policy"
            )
        });
    };
    if !agent.known {
        return unavailable(format!(
            "the reattached session did not say which modes it has, so mode '{mode}' cannot be set and checked"
        ));
    }
    if !agent.any() {
        return unavailable(format!(
            "it lists no session modes, and its modes map names '{mode}' for {policy}"
        ));
    }
    let Some(via) = agent.channel(mode) else {
        return unavailable(format!(
            "it does not offer mode '{mode}' (it offers {}), which its modes map names for {policy}",
            agent.offered().join(", ")
        ));
    };
    if agent.current.as_deref() == Some(mode) {
        return ModeStep::Keep(mode.to_string());
    }
    ModeStep::Set {
        mode: mode.to_string(),
        via,
    }
}

/// Whether the answer to setting `mode` shows that it took. `Err` says why
/// not. A `session/set_config_option` answer carries the option list, whose
/// mode option is the read-back; `session/set_mode` answers `{}` (also,
/// in vibe-acp 2.26.1, for a mode it does not list, which is why only a
/// listed one is ever sent) and is taken at its word.
pub fn read_back(
    harness: &str,
    policy: PermissionPolicy,
    mode: &str,
    via: &SetVia,
    answer: Result<&SessionOptions, String>,
) -> Result<(), String> {
    let not = |why: String| {
        Err(format!(
            "policy '{policy}' is not available for {harness}: {why}"
        ))
    };
    let options = match answer {
        Err(e) => return not(format!("it did not take mode '{mode}' ({e})")),
        Ok(o) => o,
    };
    match via {
        SetVia::SetMode => Ok(()),
        SetVia::Option(_) => match options.current_mode.as_deref() {
            Some(m) if m == mode => Ok(()),
            Some(other) => not(format!("it was set to mode '{mode}' and is in '{other}'")),
            None => not(format!(
                "its answer to setting mode '{mode}' did not say which mode it is in"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionPolicy::*;
    use serde_json::json;

    fn vibe_new() -> (Value, SessionOptions) {
        // vibe-acp 2.26.1's `session/new` (`fixtures/vibe_acp_mode.jsonl`).
        let result = json!({"sessionId": "s",
            "modes": {"currentModeId": "accept-edits", "availableModes": [
                {"id": "ask"}, {"id": "accept-edits"}, {"id": "auto-approve"}]},
            "configOptions": [{"id": "mode", "category": "mode", "type": "select",
                "currentValue": "accept-edits", "options": [
                    {"value": "ask"}, {"value": "accept-edits"}, {"value": "auto-approve"}]}]});
        let options =
            SessionOptions::from_config_options(result.get("configOptions"), None).unwrap();
        (result, options)
    }

    fn vibe_map() -> ModeMap {
        ModeMap::new([(Ask, "ask"), (Bypass, "auto-approve")])
    }

    #[test]
    fn a_policy_takes_its_entry_or_the_nearest_less_permissive_one() {
        let map = vibe_map();
        assert_eq!(map.for_policy(Plan), None);
        assert_eq!(map.for_policy(Ask), Some((Ask, "ask")));
        assert_eq!(map.for_policy(AcceptEdits), Some((Ask, "ask")));
        assert_eq!(map.for_policy(Auto), Some((Ask, "ask")));
        assert_eq!(map.for_policy(Bypass), Some((Bypass, "auto-approve")));
        assert!(map.within(Bypass, "ask") && map.within(Bypass, "auto-approve"));
        assert!(map.within(AcceptEdits, "ask"));
        assert!(!map.within(AcceptEdits, "auto-approve"));
        assert!(!map.within(Ask, "accept-edits"));
        assert!(map.names(Ask) && map.names(Bypass));
        assert!(!map.names(AcceptEdits) && !map.names(Auto));
    }

    /// vibe's `auto` is its smart-approve mode (#174): under auto the
    /// agent is set to it; under accept-edits it stays in `ask`, never in
    /// the more permissive smart-approve.
    #[test]
    fn auto_has_a_mode_of_its_own() {
        let map = ModeMap::new([
            (Ask, "ask"),
            (Auto, "smart-approve"),
            (Bypass, "auto-approve"),
        ]);
        assert_eq!(map.for_policy(AcceptEdits), Some((Ask, "ask")));
        assert_eq!(map.for_policy(Auto), Some((Auto, "smart-approve")));
        assert!(!map.within(AcceptEdits, "smart-approve"));
        assert!(map.within(Auto, "smart-approve") && map.within(Auto, "ask"));
        assert!(!map.within(Auto, "auto-approve"));
        // Started with `VIBE_DEFAULT_AGENT=smart-approve`, the session is in
        // it already (`fixtures/vibe_acp_auto.jsonl`): nothing is sent.
        let in_it = AgentModes {
            known: true,
            current: Some("smart-approve".into()),
            listed: vec![
                "ask".into(),
                "accept-edits".into(),
                "smart-approve".into(),
                "auto-approve".into(),
            ],
            option: Some("mode".into()),
            option_values: vec![
                "ask".into(),
                "accept-edits".into(),
                "smart-approve".into(),
                "auto-approve".into(),
            ],
        };
        assert_eq!(
            mode_step("vibe", Auto, &map, &in_it),
            ModeStep::Keep("smart-approve".into())
        );
        // A policy change back to ask sets `ask` through the option.
        assert_eq!(
            mode_step("vibe", Ask, &map, &in_it),
            ModeStep::Set {
                mode: "ask".into(),
                via: SetVia::Option("mode".into())
            }
        );
        // A session that does not offer it (vibe outside the experiment's
        // cohort without `VIBE_SMART_APPROVE_AVAILABLE`, or a user's
        // `disabled_agents`): auto is refused, and the TUI falls back to
        // accept-edits.
        let (result, options) = vibe_new();
        let without = AgentModes::from_session(&result, &options, false);
        assert!(matches!(
            mode_step("vibe", Auto, &map, &without),
            ModeStep::Refuse(why) if why.contains("does not offer mode 'smart-approve'")
        ));
    }

    #[test]
    fn the_map_is_read_from_config() {
        let table: BTreeMap<String, String> = [("ask", "ask"), ("accept_edits", "ask")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let map = ModeMap::from_config(&table).unwrap();
        assert_eq!(map, ModeMap::new([(Ask, "ask"), (AcceptEdits, "ask")]));
        let bad = |k: &str, v: &str| {
            ModeMap::from_config(&[(k.to_string(), v.to_string())].into()).unwrap_err()
        };
        assert!(bad("asks", "ask").contains("unknown policy 'asks'"));
        assert!(bad("ask", " ").contains("names no mode"));
    }

    #[test]
    fn the_mode_is_set_through_the_option_that_reads_it_back() {
        let (result, options) = vibe_new();
        let agent = AgentModes::from_session(&result, &options, false);
        assert_eq!(agent.current.as_deref(), Some("accept-edits"));
        assert_eq!(agent.offered(), ["ask", "accept-edits", "auto-approve"]);
        assert_eq!(
            mode_step("vibe", Ask, &vibe_map(), &agent),
            ModeStep::Set {
                mode: "ask".into(),
                via: SetVia::Option("mode".into())
            }
        );
        // Without the option, `session/set_mode` of a listed mode.
        let bare = AgentModes {
            option: None,
            option_values: vec![],
            ..agent.clone()
        };
        assert_eq!(
            mode_step("vibe", Ask, &vibe_map(), &bare),
            ModeStep::Set {
                mode: "ask".into(),
                via: SetVia::SetMode
            }
        );
        // Already in it: nothing to send.
        let asking = AgentModes {
            current: Some("ask".into()),
            ..agent.clone()
        };
        assert_eq!(
            mode_step("vibe", AcceptEdits, &vibe_map(), &asking),
            ModeStep::Keep("ask".into())
        );
    }

    #[test]
    fn ask_without_a_map_is_refused_for_an_agent_with_modes() {
        let (result, options) = vibe_new();
        let agent = AgentModes::from_session(&result, &options, false);
        let none = ModeMap::default();
        match mode_step("vibe", Ask, &none, &agent) {
            ModeStep::Refuse(why) => {
                assert!(
                    why.starts_with("policy 'ask' is not available for vibe"),
                    "{why}"
                );
                assert!(why.contains("[harnesses.vibe] has no `modes` map"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        // So is every policy below bypass: the mode the agent is in may
        // run what it asks about. Only bypass leaves the mode alone.
        for policy in [Plan, AcceptEdits, Auto] {
            match mode_step("vibe", policy, &none, &agent) {
                ModeStep::Refuse(why) => {
                    let head = format!("policy '{policy}' is not available for vibe");
                    let map = format!("no `modes` map to say which one holds to {policy}");
                    assert!(why.starts_with(&head) && why.contains(&map), "{why}");
                }
                other => panic!("{policy}: {other:?}"),
            }
        }
        assert_eq!(mode_step("vibe", Bypass, &none, &agent), ModeStep::Leave);
        // An agent with no modes asks or does not by itself.
        let modeless = AgentModes {
            known: true,
            ..Default::default()
        };
        assert_eq!(mode_step("x", Ask, &none, &modeless), ModeStep::Leave);
        // With a map, a policy with nothing at or below it is refused.
        let only_bypass = ModeMap::new([(Bypass, "auto-approve")]);
        assert!(matches!(
            mode_step("vibe", AcceptEdits, &only_bypass, &agent),
            ModeStep::Refuse(why) if why.contains("names no mode for accept-edits")
        ));
    }

    #[test]
    fn a_mode_that_cannot_be_checked_or_is_not_offered_is_refused() {
        let (result, options) = vibe_new();
        let agent = AgentModes::from_session(&result, &options, false);
        // `plan` is hidden by vibe's runtime; set_mode would answer `{}`.
        let plan = ModeMap::new([(Ask, "plan")]);
        assert!(matches!(
            mode_step("vibe", Ask, &plan, &agent),
            ModeStep::Refuse(why) if why.contains("does not offer mode 'plan' (it offers ask, accept-edits, auto-approve)")
        ));
        // A reattach that answered `{}` (vibe's `session/resume`).
        let resumed = AgentModes::from_session(&json!({}), &SessionOptions::default(), true);
        assert!(!resumed.known);
        assert!(matches!(
            mode_step("vibe", Ask, &vibe_map(), &resumed),
            ModeStep::Refuse(why) if why.contains("did not say which modes")
        ));
        // A new session that lists none.
        let none = AgentModes::from_session(
            &json!({"sessionId": "s"}),
            &SessionOptions::default(),
            false,
        );
        assert!(matches!(
            mode_step("x", Ask, &vibe_map(), &none),
            ModeStep::Refuse(why) if why.contains("lists no session modes")
        ));
        // A loaded one that does say (`fixtures/vibe_acp_mode_load.jsonl`).
        let loaded = AgentModes::from_session(&result, &options, true);
        assert!(loaded.known);
    }

    #[test]
    fn the_read_back_decides() {
        let via = SetVia::Option("mode".into());
        let with = |current: Option<&str>| SessionOptions {
            mode_option: Some("mode".into()),
            current_mode: current.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(
            read_back("vibe", Ask, "ask", &via, Ok(&with(Some("ask")))),
            Ok(())
        );
        let differs = read_back("vibe", Ask, "ask", &via, Ok(&with(Some("accept-edits"))));
        assert!(differs.unwrap_err().contains("is in 'accept-edits'"));
        assert!(read_back("vibe", Ask, "ask", &via, Ok(&with(None))).is_err());
        let refused = read_back(
            "vibe",
            Ask,
            "plan",
            &via,
            Err("Unsupported config option mode='plan'".into()),
        );
        assert!(refused.unwrap_err().contains("did not take mode 'plan'"));
        // `session/set_mode` answers `{}`: taken at its word.
        assert_eq!(
            read_back(
                "vibe",
                Ask,
                "ask",
                &SetVia::SetMode,
                Ok(&SessionOptions::default())
            ),
            Ok(())
        );
    }
}
