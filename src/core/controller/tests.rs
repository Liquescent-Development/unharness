use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::core::PermissionPolicy::{self, *};
use crate::core::SessionCommand;
use crate::harness::agy::AgyHarness;
use crate::harness::claude::ClaudeHarness;
use crate::harness::codex::{CodexHarness, CodexTransport};
use crate::harness::pi::PiHarness;

mod lifecycle;

const AGY: HarnessId = HarnessId::AGY;
const CLAUDE: HarnessId = HarnessId::CLAUDE;
const CODEX: HarnessId = HarnessId::CODEX;
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

/// What a run on Claude names: `policy` for every harness, nothing else.
fn launch(policy: Option<PermissionPolicy>) -> Launch {
    Launch {
        harness: CLAUDE,
        policy,
        provider: None,
        model: None,
        effort: None,
        default_providers: HashMap::new(),
        cwd: PathBuf::from("/"),
        workspace_root: None,
        sandbox: SandboxSetup::off(),
        rules: Rules::default(),
    }
}

/// Where a test keeps conversations: in `dir`, a directory of its own, so
/// that one which saves writes nowhere else.
fn keeping(dir: &tempfile::TempDir) -> Keeping {
    Keeping {
        store: ConversationStore::at(dir.path().to_path_buf()),
        checkpoints: None,
        context_windows: dir.path().join("context_windows.json"),
    }
}

/// A controller as a run starts, with `policy` named for every harness,
/// and the directory it keeps conversations in.
fn controller(policy: Option<PermissionPolicy>) -> (tempfile::TempDir, Controller) {
    let dir = tempfile::tempdir().unwrap();
    let c = Controller::new(registry(), Config::default(), launch(policy), keeping(&dir));
    (dir, c)
}

fn warning(c: &Controller, harness: HarnessId) -> Option<String> {
    c.policy_resolution(harness).unwrap().warning
}

#[test]
fn policy_resolution_per_harness() {
    // As with `--policy auto`: agy, which has no `auto`, runs the nearest
    // less permissive policy and says so.
    let (_dir, mut c) = controller(Some(Auto));
    assert_eq!(c.effective_policy(CLAUDE), Some(Auto));
    assert!(warning(&c, CLAUDE).is_none());
    assert_eq!(c.effective_policy(AGY), Some(AcceptEdits));
    assert!(warning(&c, AGY).unwrap().contains("less permissive"));

    // A policy chosen in the TUI is for the harness it was chosen on.
    assert!(c.route_policy(AGY, Bypass, false).is_ok());
    assert_eq!(c.effective_policy(CLAUDE), Some(Auto));
    assert_eq!(c.effective_policy(AGY), Some(Bypass));
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
    let (_dir, mut c) = controller(None);
    c.config = config;
    assert_eq!(c.effective_policy(AGY), Some(Bypass));
    assert_eq!(c.effective_policy(CLAUDE), Some(Ask));
    assert_eq!(c.effective_policy(AGY), Some(Bypass));

    // A policy set in the TUI is for the harness it was set on.
    assert!(c.route_policy(CLAUDE, AcceptEdits, false).is_ok());
    assert_eq!(c.effective_policy(AGY), Some(Bypass));
    assert_eq!(c.effective_policy(CLAUDE), Some(AcceptEdits));
}

/// Only a policy the harness has can be chosen, and only a change goes to
/// a session that was prompted; each is reported back once.
#[test]
fn a_policy_is_sent_as_a_change_and_its_report_is_not_news() {
    let (_dir, mut c) = controller(None);
    let e = c.route_policy(PI, Auto, true).unwrap_err();
    assert_eq!(e.supported, vec![Ask, Bypass]);

    let set = c.route_policy(CLAUDE, AcceptEdits, true).unwrap();
    assert!(set.send && !set.was_waiting);
    assert!(!c.route_policy(CLAUDE, AcceptEdits, true).unwrap().send);
    assert!(c.route_policy(CLAUDE, Auto, true).unwrap().send);
    // Its reports, the first after the later choice: the newest stays.
    assert_eq!(c.policy_changed(CLAUDE, AcceptEdits), None);
    assert_eq!(c.policy_changed(CLAUDE, Auto), None);
    assert_eq!(c.effective_policy(CLAUDE), Some(Auto));
    // Nothing is sent to a session nothing went to yet.
    assert!(!c.route_policy(CLAUDE, Ask, false).unwrap().send);
}

/// A mode the agent goes into by itself becomes its harness's policy, and
/// the one it planned from is what its plan approval offers first.
#[test]
fn a_mode_the_agent_goes_into_is_its_policy() {
    let (_dir, mut c) = controller(None);
    c.route_policy(CLAUDE, AcceptEdits, true).unwrap();
    c.policy_changed(CLAUDE, AcceptEdits);
    assert_eq!(
        c.policy_changed(CLAUDE, Plan).as_deref(),
        Some(
            "Claude Code (claude) went into plan mode: it asks you to approve its plan before it acts"
        )
    );
    assert_eq!(c.effective_policy(CLAUDE), Some(Plan));
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
        c.policy_changed(CLAUDE, Ask).as_deref(),
        Some("Claude Code (claude) now runs under policy ask")
    );
    // Nor is the policy in effect news.
    assert_eq!(c.policy_changed(CLAUDE, Ask), None);
}

/// A refusal moves the policy in effect to the nearest less permissive
/// one, which a live session is told, or leaves the choice to the user;
/// it is forgotten with the session.
#[test]
fn a_refused_policy_gives_way_until_the_session_ends() {
    let (_dir, mut c) = controller(Some(AcceptEdits));
    let refuse = |c: &mut Controller, policies: &[PermissionPolicy], alive| {
        c.report(
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
    assert_eq!(c.policy_changed(CLAUDE, Ask), None);
    assert_eq!(
        refuse(&mut c, &PermissionPolicy::ALL, false).refused,
        Some(Refused::Waits)
    );
    assert_eq!(c.effective_policy(CLAUDE), None);
    assert!(c.drop_refusals(CLAUDE));
    assert!(!c.drop_refusals(CLAUDE));
    assert_eq!(c.effective_policy(CLAUDE), Some(AcceptEdits));
}

/// A policy named in the run holds over what a resumed conversation had,
/// and what the run chose over both; without one the conversation's
/// choices and named policy come back, and are kept with it again.
#[test]
fn a_resumed_conversation_gives_way_to_what_the_run_named() {
    let mut conv = Conversation::new(CLAUDE);
    conv.policy = Some(Ask);
    conv.policy_choices.insert(AGY, AcceptEdits);
    conv.policy_choices.insert(CLAUDE, Bypass);

    let (_dir, mut c) = controller(None);
    c.route_policy(PI, Bypass, false).unwrap();
    c.take_conversation(&conv);
    assert_eq!(c.effective_policy(AGY), Some(AcceptEdits));
    assert_eq!(c.effective_policy(CLAUDE), Some(Bypass));
    assert_eq!(c.effective_policy(PI), Some(Bypass));
    assert_eq!(c.policy.source(CLAUDE), PolicySource::ChosenForConversation);
    assert_eq!(c.policy.source(PI), PolicySource::ChosenInRun);
    assert_eq!(c.policy.source(CODEX), PolicySource::NamedForConversation);
    c.sync_conversation(CLAUDE, false);
    let saved = &c.conversation.record;
    assert_eq!(saved.policy, Some(Ask));
    assert_eq!(saved.policy_choices.len(), 3);
    assert!(saved.last_policies.is_empty());
    c.sync_conversation(CLAUDE, true);
    let saved = &c.conversation.record;
    assert_eq!(saved.last_policies[&CLAUDE], Bypass);

    // `--policy accept-edits`: over every choice of the conversation's.
    let (_dir, mut c) = controller(Some(AcceptEdits));
    c.take_conversation(&conv);
    assert_eq!(c.policy.chosen(), HashMap::new());
    assert_eq!(c.effective_policy(CLAUDE), Some(AcceptEdits));
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

/// A run on Codex configured on `provider` with `configured-model`, with
/// `model` and `effort` named on its command line.
fn codex_run(model: Option<&str>, effort: Option<&str>, provider: &str) -> (Config, Launch) {
    let mut config = Config::default();
    config.harnesses.insert(
        "codex".into(),
        crate::config::HarnessSettings {
            default_provider: Some(provider.into()),
            default_model: Some("configured-model".into()),
            ..Default::default()
        },
    );
    let launch = Launch {
        harness: CODEX,
        model: model.map(Into::into),
        effort: effort.map(Into::into),
        ..launch(None)
    };
    (config, launch)
}

/// What a conversation keeps of a run on Codex that chose `model` and
/// `effort` where given.
fn codex_conversation(model: Option<&str>, effort: Option<&str>) -> Conversation {
    let (config, launch) = codex_run(None, None, "openai");
    let dir = tempfile::tempdir().unwrap();
    let mut c = Controller::new(registry(), config, launch, keeping(&dir));
    if let Some(m) = model {
        c.choose_model(CODEX, m.into());
    }
    if let Some(e) = effort {
        c.choose_effort(CODEX, e.into());
    }
    c.sync_conversation(CODEX, true);
    c.conversation.record.clone()
}

/// `conversation` resumed by a run as `codex_run` has it, and what it says.
fn resume_codex(
    conversation: &Conversation,
    model: Option<&str>,
    effort: Option<&str>,
    provider: &str,
) -> (tempfile::TempDir, Controller, Vec<String>) {
    let (config, launch) = codex_run(model, effort, provider);
    let dir = tempfile::tempdir().unwrap();
    let mut c = Controller::new(registry(), config, launch, keeping(&dir));
    let notices = c.take_conversation(conversation);
    (dir, c, notices)
}

fn codex_model(c: &Controller) -> Option<&str> {
    c.model(CODEX).map(|m| m.model.as_str())
}

#[test]
fn a_resumed_conversation_keeps_the_model_and_effort_chosen_for_it() {
    let conv = codex_conversation(Some("gpt-chosen"), Some("high"));
    let (_dir, c, _) = resume_codex(&conv, None, None, "openai");
    assert_eq!(codex_model(&c), Some("gpt-chosen"));
    assert_eq!(c.effort(CODEX), Some("high"));

    // Named on the command line, they win.
    let (_dir, c, _) = resume_codex(&conv, Some("gpt-flag"), Some("low"), "openai");
    assert_eq!(codex_model(&c), Some("gpt-flag"));
    assert_eq!(c.effort(CODEX), Some("low"));
}

#[test]
fn a_configured_model_is_not_kept_with_the_conversation() {
    let conv = codex_conversation(None, None);
    let (_dir, c, _) = resume_codex(&conv, None, None, "openai");
    assert!(conv.models.is_empty());
    assert!(conv.efforts.is_empty());
    assert_eq!(codex_model(&c), Some("configured-model"));
}

#[test]
fn a_model_chosen_on_another_provider_is_left() {
    let conv = codex_conversation(Some("gpt-chosen"), Some("high"));
    let (_dir, c, notices) = resume_codex(&conv, None, None, "ollama");
    assert_eq!(codex_model(&c), Some("configured-model"));
    // Its effort was for it.
    assert_eq!(c.effort(CODEX), None);
    assert!(notices.iter().any(|n| n
        == "the model chosen for Codex in this conversation (openai/gpt-chosen) is not used: Codex is on ollama now"));
}

/// A chosen provider is what the harness is told; one its session reports
/// otherwise is where it runs, and the guess gives way to what it reports.
#[test]
fn a_reported_provider_corrects_a_guess_and_not_a_choice() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Controller::new(
        registry(),
        Config::default(),
        Launch {
            model: Some("opus".into()),
            default_providers: [(CLAUDE, "anthropic".into())].into(),
            ..launch(None)
        },
        keeping(&dir),
    );
    assert_eq!(c.chosen_provider(CLAUDE), None);
    let report = |c: &mut Controller, provider: &str| {
        c.report(
            CLAUDE,
            CapsUpdate {
                provider: Some(provider.into()),
                ..Default::default()
            },
            true,
        )
        .notices
    };
    assert_eq!(
        report(&mut c, "vertex"),
        vec!["Claude runs on vertex, chosen by its own configuration".to_string()]
    );
    assert_eq!(c.provider(CLAUDE), Some(&"vertex".into()));
    assert_eq!(c.model(CLAUDE).unwrap().label(), "vertex/opus");
    assert_eq!(c.chosen_provider(CLAUDE), None);

    c.conversation
        .record
        .models
        .insert(CLAUDE, ModelRef::new(CLAUDE, "vertex", "opus"));
    assert!(c.choose_provider(CLAUDE, "bedrock".into()));
    assert!(c.conversation.record.models.is_empty() && c.model(CLAUDE).is_none());
    assert!(report(&mut c, "anthropic").is_empty());
    assert_eq!(c.runs_on(CLAUDE), Some(&"anthropic".into()));
    assert_eq!(c.chosen_provider(CLAUDE), Some(&"bedrock".into()));
    assert!(report(&mut c, "bedrock").is_empty());
    assert_eq!(c.runs_on(CLAUDE), None);
    report(&mut c, "anthropic");
    c.process_ended(CLAUDE);
    assert_eq!(c.runs_on(CLAUDE), None);
    // The same again changes nothing.
    assert!(!c.choose_provider(CLAUDE, "bedrock".into()));
}

/// When a choice may be made, and how it reaches the session it meets: the
/// one reader of `between_turns` and of `stored_session_keeps` each, and
/// the refusals' words.
#[test]
fn a_choice_is_refused_or_routed_by_the_session_it_meets() {
    use crate::harness::vibe::{KEEPS_CHOICES, VibeHarness};
    const VIBE: HarnessId = HarnessId::VIBE;
    let registry = Arc::new(
        Registry::empty()
            .with(Box::new(VibeHarness::new()))
            .with(Box::new(ClaudeHarness::default())),
    );
    let vibe = registry.get(VIBE).unwrap().descriptor().short_name;
    let dir = tempfile::tempdir().unwrap();
    let mut c = Controller::new(
        registry,
        Config::default(),
        Launch {
            harness: VIBE,
            ..launch(None)
        },
        keeping(&dir),
    );
    assert!(c.between_turns(VIBE, Choice::Model));
    assert!(!c.between_turns(CLAUDE, Choice::Effort));
    assert_eq!(c.stored_session_keeps(VIBE, true), Some(KEEPS_CHOICES));
    assert_eq!(c.stored_session_keeps(VIBE, false), None);
    assert_eq!(c.stored_session_keeps(CLAUDE, true), None);
    let prompted = SessionNow {
        alive: true,
        prompted: true,
        stored: true,
        ..Default::default()
    };

    // Mid-turn, or with subagents at work: refused, and nothing chosen.
    let turn = SessionNow {
        turn: true,
        ..prompted
    };
    assert_eq!(
        c.route_model(VIBE, "glm-5-3".into(), &turn).unwrap_err(),
        "finish or interrupt the current turn before changing the model"
    );
    let busy = SessionNow {
        subagents: 2,
        ..prompted
    };
    assert_eq!(
        c.route_model(VIBE, "glm-5-3".into(), &busy).unwrap_err(),
        "2 subagent(s) still at work would be stopped; wait for them before changing the model"
    );
    assert_eq!(c.model(VIBE), None);

    // Between turns it is sent, and held until the session says how it
    // took it; until then another is refused, as is what would end it.
    let (m, route) = c
        .route_model(
            VIBE,
            "glm-5-3".into(),
            &SessionNow {
                queued: 1,
                ..prompted
            },
        )
        .unwrap();
    assert_eq!(route, Route::Hold(SessionCommand::SetModel(m)));
    assert!(c.choice_pending());
    let taking = format!("{vibe} is still taking the model just chosen; try again in a moment");
    assert_eq!(c.settling(VIBE, false), Some(taking.clone()));
    assert_eq!(
        c.route_provider(VIBE, "lan".into(), &prompted).unwrap_err(),
        taking
    );
    let answered = c.pending_answered().unwrap();
    assert_eq!((answered.queued, answered.what), (1, "model"));
    assert_eq!(c.settling(VIBE, false), None);

    // With none prompted, a stored session keeps what it ran: a new one.
    // (Its levels are what its sessions report.)
    c.report(
        VIBE,
        CapsUpdate::efforts(vec!["off".into(), "high".into()]),
        false,
    );
    let stored = SessionNow {
        stored: true,
        ..Default::default()
    };
    let (_, route) = c.route_effort(VIBE, "OFF".into(), &stored).unwrap();
    assert_eq!(route, Route::Renew(KEEPS_CHOICES));
    assert_eq!(c.effort(VIBE), Some("off"));
    // And none is made while the vibe before it is still exiting.
    let held = SessionNow {
        start_held: true,
        ..stored
    };
    assert_eq!(
        c.route_model(VIBE, "mistral-medium-3.5".into(), &held)
            .unwrap_err(),
        format!("the previous {vibe} is still exiting; try again in a moment")
    );
    // Sent nothing and nothing stored: started again; none live: the next.
    let blank = SessionNow {
        alive: true,
        ..Default::default()
    };
    assert_eq!(
        c.route_model(VIBE, "mistral-medium-3.5".into(), &blank)
            .unwrap()
            .1,
        Route::Restart
    );
    assert_eq!(
        c.route_model(VIBE, "glm-5-3".into(), &SessionNow::default())
            .unwrap()
            .1,
        Route::None
    );

    // Claude takes its effort any time and its provider between turns,
    // and a held start does not hold the provider back.
    assert_eq!(
        c.route_effort(CLAUDE, "high".into(), &prompted).unwrap().1,
        Route::Send(SessionCommand::SetEffort(Some("high".into())))
    );
    assert!(
        c.route_effort(CLAUDE, "loud".into(), &prompted)
            .unwrap_err()
            .starts_with("unknown effort 'loud'; Claude supports: ")
    );
    assert_eq!(
        c.route_provider(CLAUDE, "bedrock".into(), &prompted)
            .unwrap(),
        Route::Hold(SessionCommand::SetProvider("bedrock".into()))
    );
    assert_eq!(
        c.pending_answered().unwrap().provider,
        Some("bedrock".into())
    );
    assert_eq!(
        c.route_provider(CLAUDE, "bedrock".into(), &prompted)
            .unwrap(),
        Route::None
    );
    let held = SessionNow {
        start_held: true,
        stored: true,
        ..Default::default()
    };
    assert_eq!(
        c.route_provider(CLAUDE, "vertex".into(), &held).unwrap(),
        Route::None
    );
    assert_eq!(c.chosen_provider(CLAUDE), Some(&"vertex".into()));
}

/// A conversation as the store gives it back, with something in each
/// field `ConversationState` takes from it.
fn saved_conversation() -> Conversation {
    use crate::core::conversations::{BlockRecord, CheckpointRecord, TurnAnchorRecord};
    let mut c = Conversation::new(CODEX);
    c.title = "the task".into();
    c.sessions.insert(CLAUDE, "claude-1".into());
    c.sessions.insert(CODEX, "codex-1".into());
    c.bookmarks.insert(CLAUDE, 2);
    c.blocks = vec![
        BlockRecord::User {
            text: "the task".into(),
            steer: false,
        },
        BlockRecord::Assistant {
            text: "done".into(),
            sender: "claude".into(),
            secs: Some(1.5),
        },
        BlockRecord::User {
            text: "and then".into(),
            steer: false,
        },
    ];
    c.usage.insert(
        CODEX,
        crate::core::Usage {
            input: 7,
            ..Default::default()
        },
    );
    c.turn_usage.insert(
        CODEX,
        crate::core::Usage {
            input: 3,
            ..Default::default()
        },
    );
    c.context.insert(
        CODEX,
        crate::core::conversations::ContextRecord {
            used: Some(30_000),
            window: Some(200_000),
            model: "default".into(),
            session: Some("codex-1".into()),
        },
    );
    c.plan.push(crate::core::PlanEntry {
        text: "write the tests".into(),
        status: crate::core::PlanStatus::InProgress,
    });
    c.anchors.push(TurnAnchorRecord {
        block: 0,
        harness: CLAUDE,
        id: "u1".into(),
        told_from: Some(0),
    });
    c.checkpoints.push(CheckpointRecord {
        block: 2,
        commit: "abc".into(),
    });
    c.fork_pending = vec![CLAUDE, CODEX];
    c
}

/// A conversation installed and saved again is what was saved: nothing
/// is lost or added between the store and the controller.
#[test]
fn a_conversation_installed_is_saved_as_it_came() {
    let saved = saved_conversation();
    let (_dir, mut c) = controller(None);
    c.install(saved.clone(), CODEX);
    let state = &c.conversation;
    assert_eq!(state.session_ids, saved.sessions);
    assert_eq!(state.transcript.blocks.len(), 3);
    assert_eq!(state.session_usage.input, 7);
    assert_eq!(state.turn_usage.input, 3);
    assert_eq!(
        state.context,
        crate::core::ContextUsage {
            used: Some(30_000),
            window: Some(200_000)
        }
    );
    assert_eq!(state.first_prompt.as_deref(), Some("the task"));
    assert!(state.fork_pending.contains(&CLAUDE) && state.fork_pending.contains(&CODEX));

    // Saved again from what the controller holds, not from the record it
    // was given: only the conversation's identity is kept from that.
    c.conversation.record = Conversation {
        id: saved.id.clone(),
        created_at: saved.created_at.clone(),
        ..Conversation::new(CODEX)
    };
    c.sync_conversation(CODEX, false);
    let again = Conversation {
        updated_at: saved.updated_at.clone(),
        ..c.conversation.record.clone()
    };
    assert_eq!(again, saved);
}

/// A conversation that cannot be written is said once in a run, not at
/// every turn.
#[test]
fn a_save_that_fails_is_said_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("conversations"), "in the way").unwrap();
    let mut c = Controller::new(
        registry(),
        Config::default(),
        launch(None),
        Keeping {
            store: ConversationStore::at(dir.path().to_path_buf()),
            checkpoints: None,
            context_windows: dir.path().join("context_windows.json"),
        },
    );
    c.install(saved_conversation(), CODEX);
    for _ in 0..3 {
        c.persist();
    }
    let said = c
        .conversation
        .transcript
        .blocks
        .iter()
        .filter(|b| {
            matches!(b, crate::core::transcript::Block::Error(e)
                if e.starts_with("could not save the conversation"))
        })
        .count();
    assert_eq!(said, 1);
}
