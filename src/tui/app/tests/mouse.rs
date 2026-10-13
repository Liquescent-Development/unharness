use super::*;
use crossterm::event::MouseButton;

#[test]
fn wheel_scrolls_the_transcript_not_the_prompt() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.history.push("earlier prompt");
    app.insert_str("draft");
    app.scroll = 20;
    app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
    assert_eq!((app.scroll, app.auto_scroll), (17, false));
    app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 5));
    assert_eq!(app.scroll, 20);
    assert_eq!(app.input, "draft");

    // Not while a dialog is open, which takes no click either.
    app.open_policy_picker();
    app.handle_mouse(mouse(MouseEventKind::ScrollUp, 5, 5));
    assert_eq!(app.scroll, 20);
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 5));
    assert!(app.selection.is_none());
}

#[test]
fn a_release_under_a_dialog_ends_the_drag_it_cut_short() {
    let left = MouseButton::Left;
    let mut app = test_app(HarnessId::CLAUDE);
    // Held past the bottom edge, or by the scrollbar thumb, when a
    // request opened a dialog.
    app.drag_edge = 1;
    app.scrollbar_grab = Some(2);
    app.open_policy_picker();
    app.handle_mouse(mouse(MouseEventKind::Drag(left), 5, 5));
    assert!(app.scrollbar_held());
    app.handle_mouse(mouse(MouseEventKind::Up(left), 5, 5));
    assert_eq!(app.drag_edge, 0);
    assert!(!app.scrollbar_held());
    assert!(app.take_copy_request().is_none());
    assert!(app.modal.is_some());
}
