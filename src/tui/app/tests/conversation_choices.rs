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

#[test]
fn a_saved_policy_the_harness_lacks_is_never_loosened() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let mut conv = Conversation::new(HarnessId::AGY);
    conv.blocks
        .push(crate::core::conversations::BlockRecord::User {
            text: "hi".into(),
            steer: false,
        });
    conv.sessions.insert(HarnessId::AGY, "agy-1".into());
    conv.policy = Some(PermissionPolicy::Ask);
    store.save(&conv).unwrap();
    let app = resume_with(tmp.path(), None);
    assert_eq!(app.controller.active, HarnessId::AGY);
    assert_eq!(app.effective_policy(), None);
    assert!(matches!(app.modal, Some(Modal::Policy(_))));
}

#[test]
fn a_policy_named_on_resume_holds_over_every_choice_even_the_same() {
    let tmp = tempfile::tempdir().unwrap();
    let store = ConversationStore::open(Some(tmp.path()), tmp.path());
    let mut conv = Conversation::new(HarnessId::AGY);
    conv.blocks
        .push(crate::core::conversations::BlockRecord::User {
            text: "hi".into(),
            steer: false,
        });
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
