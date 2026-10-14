use super::*;

fn index(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

#[test]
fn at_completes_a_file_where_the_cursor_is() {
    let mut app = test_app(HarnessId::CODEX);
    app.insert_str("see  for details");
    app.cursor = 4;
    app.insert_char('@');
    // The first `@` asks for the files; there are none to show yet.
    assert_eq!(app.completing_file, Some(4));
    assert!(app.suggestions.is_empty());
    assert_eq!(
        app.take_file_index_request(),
        Some(app.controller.cwd.clone())
    );
    app.insert_char('m');
    assert_eq!(app.take_file_index_request(), None);

    app.set_file_index(index(&["README.md", "src/main.rs", "src/tui/mod.rs"]));
    let listed: Vec<&str> = app.suggestions.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(listed.len(), 3);
    app.insert_char('a');
    assert_eq!(app.suggestions[0].0, "src/main.rs");
    assert!(app.should_accept_suggestion());

    app.accept_suggestion();
    // Whitespace followed already: none is added.
    assert_eq!(app.input, "see src/main.rs for details");
    assert_eq!(app.cursor, 15);
    assert!(app.suggestions.is_empty() && app.completing_file.is_none());

    // At the end of the input a space follows the path.
    app.take_input();
    app.insert_str("read ");
    app.insert_char('@');
    assert!(app.take_file_index_request().is_some());
    for c in "read".chars() {
        app.insert_char(c);
    }
    app.accept_suggestion();
    assert_eq!(app.input, "read README.md ");
    assert_eq!(app.cursor, 15);
}

#[test]
fn file_list_keeps_to_typed_at_words() {
    let mut app = test_app(HarnessId::CODEX);
    app.set_file_index(index(&["a.rs", "b.rs", "c.rs"]));

    // An address is not a file, and a slash command keeps its own list.
    for c in "git@a".chars() {
        app.insert_char(c);
    }
    assert!(app.suggestions.is_empty());
    app.set_input("/");
    app.insert_char('@');
    assert!(app.completing_file.is_none());

    // Pasted text and text from the editor open no list.
    app.take_input();
    app.take_file_index_request();
    app.set_file_index(index(&["a.rs", "b.rs", "c.rs"]));
    app.insert_str("@a");
    assert!(app.suggestions.is_empty());
    app.set_input("look at @a");
    assert!(app.suggestions.is_empty());
    // Nor do they have the files listed.
    assert_eq!(app.take_file_index_request(), None);

    // Typing on does, and the cursor leaving the word closes it.
    app.insert_char('.');
    assert_eq!(app.suggestions.len(), 1);
    app.move_cursor_left();
    assert!(app.suggestions.is_empty() && app.completing_file.is_none());
    app.move_cursor_end();
    app.delete_backwards();
    assert_eq!(app.suggestions.len(), 1);
    app.take_input();
    assert!(app.suggestions.is_empty() && app.completing_file.is_none());
}

#[test]
fn new_file_list_leaves_the_selection_on_its_file() {
    let mut app = test_app(HarnessId::CODEX);
    app.set_file_index(index(&["a.rs", "b.rs"]));
    app.insert_char('@');
    app.suggestion_down();
    assert_eq!(app.suggestions[app.selected_suggestion].0, "b.rs");

    app.set_file_index(index(&["0.rs", "a.rs", "b.rs"]));
    assert_eq!(app.suggestions.len(), 3);
    assert_eq!(app.suggestions[app.selected_suggestion].0, "b.rs");
    app.set_file_index(index(&["a.rs"]));
    assert_eq!(app.selected_suggestion, 0);

    // A path typed out in full is still put into the harness's form.
    for c in "a.rs".chars() {
        app.insert_char(c);
    }
    assert!(app.should_accept_suggestion());
    app.accept_suggestion();
    assert_eq!(app.input, "a.rs ");
}

#[test]
fn file_list_survives_the_cursor_and_the_walk_going_astray() {
    let mut app = test_app(HarnessId::CODEX);
    app.prompt_width = 40;
    // Up from a word no file matches moves the cursor before its `@`.
    app.insert_str("hello\n");
    for c in "@zzz".chars() {
        app.insert_char(c);
    }
    assert!(app.move_cursor_up());
    assert!(app.completing_file.is_none());
    assert!(!app.should_accept_suggestion());

    // The rest of a word edited in its middle goes with it.
    app.take_input();
    app.set_file_index(index(&["src/main.rs"]));
    app.insert_str("see @x/tui now");
    app.cursor = 6;
    app.delete_backwards();
    app.insert_char('m');
    assert_eq!(app.input, "see @m/tui now");
    app.accept_suggestion();
    assert_eq!(app.input, "see src/main.rs now");

    // An `@` typed while the files are being listed has them listed
    // again afterwards; a listing that failed does not end that.
    app.take_input();
    app.insert_char('@');
    assert!(app.take_file_index_request().is_some());
    app.take_input();
    app.insert_char('@');
    assert_eq!(app.take_file_index_request(), None);
    app.file_walk_failed();
    assert!(app.take_file_index_request().is_some());
    app.set_file_index(index(&["a.rs"]));
    assert_eq!(app.take_file_index_request(), None);
    assert_eq!(app.suggestions.len(), 1);
}

#[test]
fn a_file_is_written_the_way_the_active_harness_reads_it() {
    let mut app = test_app(HarnessId::CLAUDE);
    app.set_file_index(index(&["src/main.rs", "my notes/a.txt"]));
    for c in "@main".chars() {
        app.insert_char(c);
    }
    app.accept_suggestion();
    assert_eq!(app.input, "@src/main.rs ");

    // Typed out in the harness's form already: Enter is free to send.
    app.take_input();
    for c in "@src/main.rs".chars() {
        app.insert_char(c);
    }
    assert!(!app.should_accept_suggestion());

    app.take_input();
    for c in "@notes".chars() {
        app.insert_char(c);
    }
    app.accept_suggestion();
    assert_eq!(app.input, "@\"my notes/a.txt\" ");

    app.take_input();
    app.switch_harness(HarnessId::PI);
    for c in "@main".chars() {
        app.insert_char(c);
    }
    app.accept_suggestion();
    assert_eq!(app.input, "src/main.rs ");
}
