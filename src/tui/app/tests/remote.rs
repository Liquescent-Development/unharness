use super::*;

#[test]
fn remote_control_is_offered_only_where_declared() {
    use super::super::transcript::Block;
    // pi runs commands of its own: one called /rc is its.
    let mut app = test_app(HarnessId::PI);
    app.controller.session_alive = true;
    app.handle_slash_command("/rc now");
    assert!(app.remote_status().is_none());
    assert!(
        app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("passed to pi as typed")))
    );
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Notice(n) if n.contains("does not see what")))
    );
    assert!(!app.offers_command("/remote-control"));
    assert!(!app.owns_command("/rc"));
    assert!(app.offers_command("/model"));
    let claude = test_app(HarnessId::CLAUDE);
    assert!(claude.owns_command("/rc"));
}
