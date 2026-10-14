use std::sync::Arc;

use super::*;
use crate::core::PermissionPolicy::{self, *};
use crate::harness::agy::AgyHarness;
use crate::harness::claude::ClaudeHarness;
use crate::harness::codex::{CodexHarness, CodexTransport};
use crate::harness::pi::PiHarness;

const AGY: HarnessId = HarnessId::AGY;
const CLAUDE: HarnessId = HarnessId::CLAUDE;
const PI: HarnessId = HarnessId::PI;

/// The harnesses the TUI's tests run on.
fn registry() -> Arc<Registry> {
    Arc::new(
        Registry::empty()
            .with(Box::new(AgyHarness))
            .with(Box::new(ClaudeHarness::default()))
            .with(Box::new(CodexHarness::new(CodexTransport::AppServer)))
            .with(Box::new(PiHarness::default())),
    )
}

/// A controller as a run starts, with `policy` named for every harness.
fn controller(policy: Option<PermissionPolicy>) -> Controller {
    Controller::new(registry(), policy)
}

fn warning(c: &Controller, config: &Config, harness: HarnessId) -> Option<String> {
    c.policy_resolution(config, harness).unwrap().warning
}

#[test]
fn policy_resolution_per_harness() {
    // As with `--policy auto`: agy, which has no `auto`, runs the nearest
    // less permissive policy and says so.
    let config = Config::default();
    let mut c = controller(Some(Auto));
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Auto));
    assert!(warning(&c, &config, CLAUDE).is_none());
    assert_eq!(c.effective_policy(&config, AGY), Some(AcceptEdits));
    assert!(
        warning(&c, &config, AGY)
            .unwrap()
            .contains("less permissive")
    );

    // A policy chosen in the TUI is for the harness it was chosen on.
    assert!(c.set_policy(&config, AGY, Bypass, false).is_ok());
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Auto));
    assert_eq!(c.effective_policy(&config, AGY), Some(Bypass));
}

#[test]
fn a_harness_default_policy_stays_with_its_harness() {
    let mut config = Config::default();
    config.harnesses.insert(
        "agy".into(),
        crate::config::HarnessSettings {
            default_policy: Some("bypass".into()),
            ..Default::default()
        },
    );
    let mut c = controller(None);
    assert_eq!(c.effective_policy(&config, AGY), Some(Bypass));
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Ask));
    assert_eq!(c.effective_policy(&config, AGY), Some(Bypass));

    // A policy set in the TUI is for the harness it was set on.
    assert!(c.set_policy(&config, CLAUDE, AcceptEdits, false).is_ok());
    assert_eq!(c.effective_policy(&config, AGY), Some(Bypass));
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(AcceptEdits));
}

/// Only a policy the harness has can be chosen, and only a change goes to
/// a session that was prompted; each is reported back once.
#[test]
fn a_policy_is_sent_as_a_change_and_its_report_is_not_news() {
    let config = Config::default();
    let mut c = controller(None);
    let e = c.set_policy(&config, PI, Auto, true).unwrap_err();
    assert_eq!(e.supported, vec![Ask, Bypass]);

    let set = c.set_policy(&config, CLAUDE, AcceptEdits, true).unwrap();
    assert!(set.send && !set.was_waiting);
    assert!(
        !c.set_policy(&config, CLAUDE, AcceptEdits, true)
            .unwrap()
            .send
    );
    assert!(c.set_policy(&config, CLAUDE, Auto, true).unwrap().send);
    // Its reports, the first after the later choice: the newest stays.
    assert_eq!(c.on_policy_changed(&config, CLAUDE, AcceptEdits), None);
    assert_eq!(c.on_policy_changed(&config, CLAUDE, Auto), None);
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Auto));
    // Nothing is sent to a session nothing went to yet.
    assert!(!c.set_policy(&config, CLAUDE, Ask, false).unwrap().send);
}

/// A mode the agent goes into by itself becomes its harness's policy, and
/// the one it planned from is what its plan approval offers first.
#[test]
fn a_mode_the_agent_goes_into_is_its_policy() {
    let config = Config::default();
    let mut c = controller(None);
    c.set_policy(&config, CLAUDE, AcceptEdits, true).unwrap();
    c.on_policy_changed(&config, CLAUDE, AcceptEdits);
    assert_eq!(
        c.on_policy_changed(&config, CLAUDE, Plan).as_deref(),
        Some(
            "Claude Code (claude) went into plan mode: it asks you to approve its plan before it acts"
        )
    );
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Plan));
    assert_eq!(c.policy.pre_plan(CLAUDE), Some(AcceptEdits));
    assert_eq!(c.policy.source(CLAUDE), PolicySource::ChosenInRun);
    // Bypass only in a session started under it.
    assert_eq!(c.plan_policies(CLAUDE), vec![Ask, AcceptEdits, Auto]);
    c.policy.started_under(Some(Bypass));
    assert_eq!(
        c.plan_policies(CLAUDE),
        vec![Ask, AcceptEdits, Auto, Bypass]
    );
    assert_eq!(
        c.on_policy_changed(&config, CLAUDE, Ask).as_deref(),
        Some("Claude Code (claude) now runs under policy ask")
    );
    // Nor is the policy in effect news.
    assert_eq!(c.on_policy_changed(&config, CLAUDE, Ask), None);
}

/// A refusal moves the policy in effect to the nearest less permissive
/// one, which a live session is told, or leaves the choice to the user;
/// it is forgotten with the session.
#[test]
fn a_refused_policy_gives_way_until_the_session_ends() {
    let config = Config::default();
    let mut c = controller(Some(AcceptEdits));
    let refuse = |c: &mut Controller, policies: &[PermissionPolicy], alive| {
        c.report(
            &config,
            CLAUDE,
            CapsUpdate {
                refused_policies: Some(policies.to_vec()),
                ..Default::default()
            },
            alive,
        )
    };
    assert_eq!(refuse(&mut c, &[Bypass], true).refused, None);
    assert_eq!(
        refuse(&mut c, &[AcceptEdits], true).refused,
        Some(Refused::GaveWay {
            now: Ask,
            send: true
        })
    );
    assert!(c.refused_policies(CLAUDE));
    // Its report is not news.
    assert_eq!(c.on_policy_changed(&config, CLAUDE, Ask), None);
    assert_eq!(
        refuse(&mut c, &PermissionPolicy::ALL, false).refused,
        Some(Refused::Waits)
    );
    assert_eq!(c.effective_policy(&config, CLAUDE), None);
    assert!(c.forget_refusals(CLAUDE));
    assert!(!c.forget_refusals(CLAUDE));
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(AcceptEdits));
}

/// A policy named in the run holds over what a resumed conversation had,
/// and what the run chose over both; without one the conversation's
/// choices and named policy come back, and are kept with it again.
#[test]
fn a_resumed_conversation_gives_way_to_what_the_run_named() {
    let config = Config::default();
    let mut conv = Conversation::new(CLAUDE);
    conv.policy = Some(Ask);
    conv.policy_choices.insert(AGY, AcceptEdits);
    conv.policy_choices.insert(CLAUDE, Bypass);

    let mut c = controller(None);
    c.set_policy(&config, PI, Bypass, false).unwrap();
    c.take_conversation(&conv);
    assert_eq!(c.effective_policy(&config, AGY), Some(AcceptEdits));
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(Bypass));
    assert_eq!(c.effective_policy(&config, PI), Some(Bypass));
    assert_eq!(c.policy.source(CLAUDE), PolicySource::ChosenForConversation);
    assert_eq!(c.policy.source(PI), PolicySource::ChosenInRun);
    assert_eq!(
        c.policy.source(HarnessId::CODEX),
        PolicySource::NamedForConversation
    );
    let mut saved = Conversation::new(CLAUDE);
    c.sync_conversation(&config, CLAUDE, false, &mut saved);
    assert_eq!(saved.policy, Some(Ask));
    assert_eq!(saved.policy_choices.len(), 3);
    assert!(saved.last_policies.is_empty());
    c.sync_conversation(&config, CLAUDE, true, &mut saved);
    assert_eq!(saved.last_policies[&CLAUDE], Bypass);

    // `--policy accept-edits`: over every choice of the conversation's.
    let mut c = controller(Some(AcceptEdits));
    c.take_conversation(&conv);
    assert_eq!(c.policy.chosen(), HashMap::new());
    assert_eq!(c.effective_policy(&config, CLAUDE), Some(AcceptEdits));
    assert_eq!(c.policy.source(CLAUDE), PolicySource::NamedInRun);
    // Another conversation does not take it away.
    c.take_conversation(&Conversation::new(CLAUDE));
    assert_eq!(c.policy.named(), Some(AcceptEdits));
}

/// A run without the TUI stops on a configured policy that does not
/// parse; the TUI, which checked it at startup, takes `ask`.
#[test]
fn a_configured_policy_that_does_not_parse() {
    let config = Config {
        default_policy: Some("nah".into()),
        ..Default::default()
    };
    let p = PolicyState::new(None);
    assert!(p.requested_checked(&config, CLAUDE).is_err());
    assert_eq!(p.requested(&config, CLAUDE), Ask);
    let p = PolicyState::new(Some(Auto));
    assert_eq!(p.requested_checked(&config, CLAUDE).unwrap(), Auto);
}
