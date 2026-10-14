use super::*;

/// Claude's configured default is `accept-edits`.
fn accept_edits_config() -> Config {
    let mut config = Config::default();
    config.harnesses.insert(
        "claude".into(),
        crate::config::HarnessSettings {
            default_policy: Some("accept-edits".into()),
            ..Default::default()
        },
    );
    config
}

/// Resume the last conversation in `cwd`, with `policy` on the command
/// line.
fn resume_with(cwd: &Path, policy: Option<PermissionPolicy>) -> App {
    let mut init = test_init(
        cwd.to_path_buf(),
        HarnessId::CLAUDE,
        Some(String::new()),
        false,
        accept_edits_config(),
        test_registry(),
    );
    init.policy = policy;
    App::new(init)
}

/// A turn on Claude under `policy`, then quit.
fn claude_turn_under(cwd: &Path, policy: Option<PermissionPolicy>) {
    let mut app = resume_with(cwd, None);
    if let Some(p) = policy {
        assert!(app.set_policy(p));
    }
    app.submit_prompt("hi".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "claude-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.quit();
}

#[test]
fn a_resumed_conversation_keeps_the_policy_it_was_run_with() {
    let tmp = tempfile::tempdir().unwrap();
    claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
    let app = resume_with(tmp.path(), None);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
    assert!(!notices(&app).iter().any(|n| n.contains("last ran")));
}

#[test]
fn a_policy_named_on_resume_wins_and_is_told() {
    let tmp = tempfile::tempdir().unwrap();
    claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
    let app = resume_with(tmp.path(), Some(PermissionPolicy::Ask));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(notices(&app).iter().any(|n| n
        == "Claude last ran this conversation under auto; it continues under ask (named with --policy)"));
}

#[test]
fn a_conversation_on_the_default_follows_the_config() {
    let tmp = tempfile::tempdir().unwrap();
    claude_turn_under(tmp.path(), None);
    let app = resume_with(tmp.path(), None);
    assert_eq!(app.conversation.policy, None);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));

    // The default changed since: the conversation follows, and says so.
    let init = test_init(
        tmp.path().to_path_buf(),
        HarnessId::CLAUDE,
        Some(String::new()),
        false,
        Config::default(),
        test_registry(),
    );
    let app = App::new(init);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(notices(&app).iter().any(|n| n
        == "Claude last ran this conversation under accept-edits; it continues under ask (the configured default)"));
}

#[test]
fn a_saved_policy_the_harness_lacks_is_never_loosened() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let mut conv = Conversation::new(HarnessId::AGY);
    conv.blocks
        .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
    conv.sessions.insert(HarnessId::AGY, "agy-1".into());
    conv.policy = Some(PermissionPolicy::Ask);
    store.save(&conv).unwrap();
    let app = resume_with(tmp.path(), None);
    assert_eq!(app.active, HarnessId::AGY);
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
}

#[test]
fn resuming_in_the_tui_takes_the_conversations_policy() {
    let tmp = tempfile::tempdir().unwrap();
    claude_turn_under(tmp.path(), Some(PermissionPolicy::Auto));
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let auto = store.last().unwrap().id;
    // One that never named a policy, last run under the default.
    let mut plain = Conversation::new(HarnessId::CLAUDE);
    plain
        .blocks
        .push(crate::core::conversations::BlockRecord::User { text: "x".into() });
    plain
        .last_policies
        .insert(HarnessId::CLAUDE, PermissionPolicy::AcceptEdits);
    store.save(&plain).unwrap();

    let mut app = test_app_with(
        tmp.path().to_path_buf(),
        HarnessId::CLAUDE,
        None,
        false,
        accept_edits_config(),
        test_registry(),
    );
    app.resume_conversation(auto);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Auto));
    // What the last one ran under does not follow into the next.
    app.resume_conversation(plain.id.clone());
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(!notices(&app).iter().any(|n| n.contains("last ran")));
    app.persist();
    assert_eq!(store.load(&plain.id).unwrap().policy, None);
}

#[test]
fn a_policy_named_in_the_run_holds_over_a_resumed_one() {
    let tmp = tempfile::tempdir().unwrap();
    claude_turn_under(tmp.path(), Some(PermissionPolicy::Bypass));
    let id = ConversationStore::open(Some(tmp.path()), tmp.path())
        .last()
        .unwrap()
        .id;
    let mut app = test_app_with(
        tmp.path().to_path_buf(),
        HarnessId::CLAUDE,
        None,
        false,
        accept_edits_config(),
        test_registry(),
    );
    assert!(app.set_policy(PermissionPolicy::Ask));
    app.resume_conversation(id);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
    assert!(notices(&app).iter().any(|n| n
        == "Claude last ran this conversation under bypass; it continues under ask (chosen in this run)"));

    // Named with `--policy`, for every harness: over what the
    // conversation chose for Claude too.
    let app = resume_with(tmp.path(), Some(PermissionPolicy::AcceptEdits));
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(notices(&app).iter().any(|n| n
        == "Claude last ran this conversation under bypass; it continues under accept-edits (named with --policy)"));
}

/// A choice the harness no longer offers is not passed off as what runs.
#[test]
fn a_resumed_choice_that_is_not_offered_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let mut conv = Conversation::new(HarnessId::AGY);
    conv.blocks
        .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
    conv.policy_choices
        .insert(HarnessId::AGY, PermissionPolicy::Ask);
    conv.last_policies
        .insert(HarnessId::AGY, PermissionPolicy::AcceptEdits);
    store.save(&conv).unwrap();
    let app = resume_with(tmp.path(), None);
    assert!(notices(&app).iter().any(|n| n
        == "Antigravity last ran this conversation under accept-edits; it waits for a policy (ask is not available on Antigravity)"), "{:?}", notices(&app));
}

#[test]
fn a_policy_named_on_resume_holds_over_every_choice_even_the_same() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let mut conv = Conversation::new(HarnessId::AGY);
    conv.blocks
        .push(crate::core::conversations::BlockRecord::User { text: "hi".into() });
    conv.policy = Some(PermissionPolicy::Ask);
    conv.policy_choices
        .insert(HarnessId::AGY, PermissionPolicy::AcceptEdits);
    conv.policy_choices
        .insert(HarnessId::CLAUDE, PermissionPolicy::Bypass);
    store.save(&conv).unwrap();
    // Resumed as it was: its choices hold.
    let app = resume_with(tmp.path(), None);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::AcceptEdits));
    assert!(app.modal.is_none());
    // `--policy ask`, the policy it was named: `ask` everywhere, so agy
    // waits for a choice and Claude does not run under `bypass`.
    let mut app = resume_with(tmp.path(), Some(PermissionPolicy::Ask));
    assert_eq!(app.controller.policy.chosen(), HashMap::new());
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
    app.modal = None;
    app.switch_harness(HarnessId::CLAUDE);
    assert_eq!(app.effective_policy(), Some(PermissionPolicy::Ask));
}

/// Resume the last conversation in `cwd` with `model` and `effort` on
/// the command line, Codex's provider configured as `provider`.
fn resume_codex_with(cwd: &Path, model: Option<&str>, effort: Option<&str>, provider: &str) -> App {
    let mut config = Config::default();
    config.harnesses.insert(
        "codex".into(),
        crate::config::HarnessSettings {
            default_provider: Some(provider.into()),
            default_model: Some("configured-model".into()),
            ..Default::default()
        },
    );
    let mut init = test_init(
        cwd.to_path_buf(),
        HarnessId::CODEX,
        Some(String::new()),
        false,
        config,
        test_registry(),
    );
    init.model = model.map(Into::into);
    init.effort = effort.map(Into::into);
    App::new(init)
}

fn codex_model(app: &App) -> Option<&str> {
    app.controller
        .model(HarnessId::CODEX)
        .map(|m| m.model.as_str())
}

/// A turn on Codex after `/model` and `/effort` where given, then quit.
fn codex_turn_choosing(cwd: &Path, model: Option<&str>, effort: Option<&str>) {
    let mut app = resume_codex_with(cwd, None, None, "openai");
    if let Some(m) = model {
        app.set_model(m.into());
    }
    if let Some(e) = effort {
        app.set_effort(e.into());
    }
    app.submit_prompt("hi".into());
    app.take_actions();
    app.session_alive = true;
    app.on_event(AgentEvent::SessionStarted {
        session_id: "codex-1".into(),
        model: None,
    });
    app.on_event(AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    });
    app.quit();
}

#[test]
fn resuming_in_the_tui_does_not_carry_one_conversations_model_into_another() {
    let tmp = tempfile::tempdir().unwrap();
    codex_turn_choosing(tmp.path(), Some("gpt-chosen"), Some("high"));
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let chosen = store.last().unwrap().id;
    let mut plain = Conversation::new(HarnessId::CODEX);
    plain
        .blocks
        .push(crate::core::conversations::BlockRecord::User { text: "x".into() });
    store.save(&plain).unwrap();

    let mut app = resume_codex_with(tmp.path(), None, None, "openai");
    app.resume_conversation(chosen.clone());
    assert_eq!(codex_model(&app), Some("gpt-chosen"));
    app.resume_conversation(plain.id.clone());
    assert_eq!(codex_model(&app), Some("configured-model"));
    assert_eq!(app.current_effort(), None);
    app.persist();
    let saved = store.load(&plain.id).unwrap();
    assert!(saved.models.is_empty() && saved.efforts.is_empty());

    // One chosen in the run holds.
    app.set_model("gpt-run".into());
    app.resume_conversation(chosen);
    assert_eq!(codex_model(&app), Some("gpt-run"));
    assert_eq!(app.current_effort(), Some("high"));
}
