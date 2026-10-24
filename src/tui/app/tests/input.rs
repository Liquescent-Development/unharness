use super::*;
use crate::tui::clipboard::Pasted;

#[test]
fn attachments_go_out_with_the_next_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("my shot.png"), b"png").unwrap();
    std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    std::fs::write(tmp.path().join("a.out"), [0xff, 0xfe]).unwrap();
    app.handle_slash_command("/attach a.out");
    app.handle_slash_command("/attach missing.png");
    assert!(app.attachments.is_empty());
    app.handle_slash_command("/attach my shot.png");
    assert_eq!(app.attachments.len(), 1);

    app.submit_prompt("what is this?".into());
    assert!(app.attachments.is_empty());
    let sent = app.take_actions().into_iter().find_map(|a| match a {
        Action::SendTurn { attachments, .. } => Some(attachments),
        _ => None,
    });
    assert_eq!(sent.unwrap()[0].label(), "my shot.png");
    assert!(matches!(
        app.controller.conversation.transcript.blocks.last(),
        Some(super::super::transcript::Block::User { text, .. }) if text.ends_with("[image: my shot.png]")
    ));

    // A harness that takes no images refuses the attachment.
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
    app.handle_slash_command("/attach my shot.png");
    assert!(app.attachments.is_empty());
}

#[test]
fn files_are_attached_where_the_harness_takes_them() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
    std::fs::write(tmp.path().join("shot.png"), b"png").unwrap();
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.handle_slash_command("/attach notes.txt");
    app.submit_prompt("summarize".into());
    let sent = app.take_actions().into_iter().find_map(|a| match a {
        Action::SendTurn { attachments, .. } => Some(attachments),
        _ => None,
    });
    assert_eq!(sent.unwrap()[0].mime(), "text/plain");
    assert!(matches!(
        app.controller.conversation.transcript.blocks.last(),
        Some(super::super::transcript::Block::User { text, .. }) if text.ends_with("[file: notes.txt]")
    ));

    // Codex takes images only.
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CODEX, None, false);
    app.handle_slash_command("/attach notes.txt");
    assert!(app.attachments.is_empty());
    // A file that got there anyway (queued under another harness) is
    // dropped at send time, not passed off as an image.
    app.handle_slash_command("/attach shot.png");
    app.attachments
        .push(Attachment::file(tmp.path().join("notes.txt")).unwrap());
    app.submit_prompt("look".into());
    let sent = app.take_actions().into_iter().find_map(|a| match a {
        Action::SendTurn { attachments, .. } => Some(attachments),
        _ => None,
    });
    assert_eq!(sent.unwrap().len(), 1);
}

#[test]
fn paste_inserts_at_the_cursor_without_submitting() {
    let mut app = test_app(HarnessId::CLAUDE);
    for c in "ab".chars() {
        app.insert_char(c);
    }
    app.move_cursor_left();
    app.paste("one\r\ntwo\rthree\n\tfour\x1b[31m");
    assert_eq!(app.input, "aone\ntwo\nthree\n    four[31mb");
    assert_eq!(app.cursor, app.input.chars().count() - 1);
    assert!(!app.controller.is_generating && app.take_actions().is_empty());
    // Pasted text that starts with a slash is not a command to complete.
    app.take_input();
    app.paste("/model\nsonnet");
    assert!(app.suggestions.is_empty());
}

#[test]
fn dropped_files_are_attached_instead_of_pasted() {
    let tmp = tempfile::tempdir().unwrap();
    let file = |name: &str, content: &[u8]| {
        let path = tmp.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
    };
    let shot = file("my shot.png", b"png");
    let notes = file("notes.txt", b"txt");
    let binary = file("a.out", &[0xff, 0xfe]);
    let labels = |app: &App| -> Vec<String> { app.attachments.iter().map(|a| a.label()).collect() };

    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.paste(&format!("'{shot}' "));
    app.paste(&format!("file://{}\n", notes.replace(' ', "%20")));
    assert_eq!(labels(&app), ["my shot.png", "notes.txt"]);
    assert!(app.input.is_empty());

    // Paths in a sentence, and paths of no file, are text.
    app.paste(&format!("see {notes}"));
    app.paste(&format!(" {notes}.missing"));
    assert_eq!(app.input, format!("see {notes} {notes}.missing"));
    assert_eq!(app.attachments.len(), 2);

    // Codex takes the image; the others stay paths, with the reason.
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CODEX, None, false);
    app.paste(&format!("{notes}\n{shot}\n{binary}"));
    assert_eq!(labels(&app), ["my shot.png"]);
    assert_eq!(app.input, format!("{notes} {binary}"));
    let notices: Vec<&str> = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            super::super::transcript::Block::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .filter(|text| text.contains("not attached"))
        .collect();
    assert_eq!(notices.len(), 2);
    assert!(
        notices[0].contains("does not accept file attachments")
            && notices[0].contains("notes.txt not attached: ")
    );
    assert!(notices[1].contains("a.out not attached: only images"));

    // Nothing it can take: the paste goes in as it came.
    app.take_input();
    app.paste(&format!("'{notes}' "));
    assert_eq!(app.input, format!("'{notes}' "));
}

#[test]
fn clipboard_images_are_saved_and_attached() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("pasted");
    let image = || {
        Ok(Pasted::Image {
            extension: "png",
            bytes: b"png".to_vec(),
        })
    };
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::CLAUDE, None, false);
    app.handle_slash_command("/paste");
    assert!(app.take_clipboard_request() && !app.take_clipboard_request());
    app.attach_pasted(image(), &dir);
    let [a] = &app.attachments[..] else {
        panic!("one attachment expected");
    };
    assert!(a.is_image() && a.path().starts_with(&dir));
    assert_eq!(std::fs::read(a.path()).unwrap(), b"png");

    // Copied files are attached where they are.
    std::fs::write(tmp.path().join("notes.txt"), b"txt").unwrap();
    app.attach_pasted(Ok(Pasted::Files(vec![tmp.path().join("notes.txt")])), &dir);
    assert_eq!(app.attachments[1].path(), tmp.path().join("notes.txt"));

    // Nothing read, and the reason, when there was nothing to read.
    app.attach_pasted(Err(anyhow::anyhow!("over ssh …")), &dir);
    assert_eq!(app.attachments.len(), 2);
    assert!(matches!(
        app.controller.conversation.transcript.blocks.last(),
        Some(super::super::transcript::Block::Error(text)) if text == "not pasted: over ssh …"
    ));

    // No image is written for a harness that takes none.
    let dir = tmp.path().join("unused");
    let mut app = test_app_in(tmp.path().to_path_buf(), HarnessId::AGY, None, false);
    app.attach_pasted(image(), &dir);
    assert!(app.attachments.is_empty() && !dir.exists());
}

#[test]
fn paste_into_a_modal_fills_its_text_field_or_is_ignored() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.controller.session_alive = true;
    app.on_event(AgentEvent::PermissionRequest(PermissionRequest {
        id: "p1".into(),
        kind: PermissionKind::ToolUse {
            tool: "Bash".into(),
            input: serde_json::json!({"command":"ls"}),
            action: crate::core::ToolAction::Opaque,
            description: None,
        },
        tool_call_id: None,
    }));
    // "y" would allow and "a" would always allow if a paste were keys.
    app.paste("yes\nalways");
    assert!(app.modal.is_some() && app.take_actions().is_empty());
    assert!(app.input.is_empty());

    app.handle_modal_key(key(KeyCode::Char('n')));
    app.paste("not\nnow");
    app.handle_modal_key(key(KeyCode::Enter));
    assert!(app.take_actions().iter().any(|a| matches!(
        a,
        Action::Command(SessionCommand::RespondPermission {
            decision: PermissionDecision::Deny { reason }, ..
        }) if reason == "not now"
    )));
}

#[test]
fn multibyte_input_editing() {
    let mut app = test_app(HarnessId::CLAUDE);
    for c in "héllo".chars() {
        app.insert_char(c);
    }
    app.move_cursor_left();
    app.move_cursor_left();
    app.delete_backwards();
    assert_eq!(app.input, "hélo");
    app.move_cursor_home();
    app.delete_forwards();
    assert_eq!(app.input, "élo");
}
