use super::*;
use crate::core::transcript::{Block, Fold, KeptAt, current_compaction, folds};
use crate::core::{CompactTrigger, Compaction, Kept};

fn done() -> AgentEvent {
    AgentEvent::TurnCompleted {
        stop_reason: StopReason::Done,
    }
}

/// A turn of `prompt`, whose anchor the harness names `anchor`, told
/// after the turn as pi tells it. The first starts the session.
fn turn(app: &mut TestApp, prompt: &str, anchor: &str) {
    app.submit_prompt(prompt.into());
    let harness = app.controller.active;
    if !app
        .controller
        .conversation
        .session_ids
        .contains_key(&harness)
    {
        app.on_event(AgentEvent::SessionStarted {
            session_id: format!("{harness}-1"),
            model: None,
        });
    }
    app.on_event(AgentEvent::TextDelta(format!("answer to {prompt}")));
    app.on_event(done());
    app.on_event(AgentEvent::TurnAnchor { id: anchor.into() });
}

fn compacted(kept: Kept) -> AgentEvent {
    AgentEvent::Compacted(Compaction {
        trigger: Some(CompactTrigger::Manual),
        tokens_before: Some(252_000),
        tokens_after: Some(52_000),
        summary: Some("## Goal\nship it".into()),
        kept,
    })
}

fn user_block(app: &TestApp, text: &str) -> usize {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::User { text: t, .. } if t == text))
        .unwrap()
}

fn compaction_at(app: &TestApp) -> usize {
    app.controller
        .conversation
        .transcript
        .blocks
        .iter()
        .rposition(|b| matches!(b, Block::Compaction { .. }))
        .unwrap()
}

fn kept(app: &TestApp) -> KeptAt {
    match &app.controller.conversation.transcript.blocks[compaction_at(app)] {
        Block::Compaction { kept, .. } => kept.clone(),
        _ => unreachable!(),
    }
}

fn shown_folds(app: &TestApp) -> Vec<Fold> {
    folds(
        &app.controller.conversation.transcript.blocks,
        app.controller.active,
    )
}

/// pi keeps the messages from a turn it names by the entry id its anchor
/// gave: the turns before it fold, that one and the rest stay.
#[test]
fn a_compaction_folds_the_turns_before_the_one_it_kept() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    turn(&mut app, "three", "e3");
    app.on_event(compacted(Kept::From("e3".into())));
    let three = user_block(&app, "three");
    assert_eq!(kept(&app), KeptAt::Block(three));
    assert_eq!(
        shown_folds(&app),
        [Fold {
            compaction: compaction_at(&app),
            end: three,
            turns: 2,
            folded: true,
        }]
    );
    let Block::Compaction {
        harness,
        summary,
        tokens_before,
        ..
    } = &app.controller.conversation.transcript.blocks[compaction_at(&app)]
    else {
        unreachable!()
    };
    assert_eq!(*harness, HarnessId::PI);
    assert_eq!(summary.as_deref(), Some("## Goal\nship it"));
    assert_eq!(*tokens_before, Some(252_000));
}

/// pi lists a turn's anchor after the turn: a compaction during the turn
/// that keeps it waits for the anchor to know where it starts.
#[test]
fn a_kept_turn_whose_anchor_is_owed_is_waited_for() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    app.submit_prompt("two".into());
    app.on_event(compacted(Kept::From("e2".into())));
    assert_eq!(kept(&app), KeptAt::Pending(vec!["e2".into()]));
    assert!(shown_folds(&app).is_empty());
    app.on_event(done());
    app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
    assert_eq!(kept(&app), KeptAt::Block(user_block(&app, "two")));
    assert_eq!(shown_folds(&app)[0].turns, 1);
}

/// What cannot be put on a turn for certain folds nothing: a message
/// inside a turn (pi's split turn), a harness that does not say (Codex).
/// A compaction that kept nothing folds all before it.
#[test]
fn only_a_boundary_known_for_certain_folds() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("6938e548".into())));
    assert_eq!(kept(&app), KeptAt::Unknown);
    app.on_event(compacted(Kept::Unknown));
    assert_eq!(kept(&app), KeptAt::Unknown);
    assert!(shown_folds(&app).is_empty());
    // Both are in the transcript all the same, divider and summary.
    let count = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::Compaction { .. }))
        .count();
    assert_eq!(count, 2);

    app.on_event(compacted(Kept::Nothing));
    let at = compaction_at(&app);
    assert_eq!(kept(&app), KeptAt::Block(at));
    assert_eq!(shown_folds(&app)[0].turns, 2);
}

/// A kept turn's message also told the harness what it had not seen of
/// the conversation (what other harnesses did, `!` commands): the model
/// has that in the message, and it is not folded.
#[test]
fn a_kept_turn_keeps_what_its_message_told() {
    let mut app = test_app(HarnessId::CLAUDE);
    turn(&mut app, "one", "u1");
    app.switch_harness(HarnessId::PI);
    // pi's first prompt tells it all of Claude's turn.
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    assert_eq!(kept(&app), KeptAt::Block(0));
    assert!(shown_folds(&app).is_empty());

    // A `!` command goes in front of the next prompt.
    turn(&mut app, "three", "e3");
    app.run_shell("true");
    let shell = app.controller.conversation.transcript.blocks.len() - 1;
    app.with(|c, front| c.shell_exited(Some(0), front));
    turn(&mut app, "four", "e4");
    app.on_event(compacted(Kept::From("e4".into())));
    assert_eq!(kept(&app), KeptAt::Block(shell));
    assert!(shell < user_block(&app, "four"));
}

/// Each compaction folds what came before what it kept; a later one's
/// fold holds the earlier one's.
#[test]
fn several_compactions_fold_one_inside_the_other() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    let first = compaction_at(&app);
    turn(&mut app, "three", "e3");
    turn(&mut app, "four", "e4");
    app.on_event(compacted(Kept::From("e4".into())));
    let second = compaction_at(&app);
    let folds = shown_folds(&app);
    assert_eq!(folds.len(), 2);
    assert_eq!((folds[0].compaction, folds[0].turns), (first, 1));
    assert_eq!((folds[1].compaction, folds[1].turns), (second, 3));
    assert!(folds[0].end < first && first < folds[1].end);
}

/// The compaction is saved with the conversation, and a resumed one is
/// folded where it was.
#[test]
fn a_compaction_is_saved_and_comes_back_folded() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_path_buf();
    let mut app = test_app_in(cwd.clone(), HarnessId::PI, None, false);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    if let Block::Compaction {
        folded, collapsed, ..
    } = app
        .controller
        .conversation
        .transcript
        .blocks
        .last_mut()
        .unwrap()
    {
        *folded = false;
        *collapsed = false;
    }
    let before = shown_folds(&app);
    app.quit();

    let again = test_app_in(cwd, HarnessId::PI, Some(String::new()), false);
    let blocks = &again.controller.conversation.transcript.blocks;
    let at = blocks
        .iter()
        .position(|b| matches!(b, Block::Compaction { .. }))
        .unwrap();
    let Block::Compaction {
        harness,
        trigger,
        tokens_before,
        tokens_after,
        summary,
        kept,
        retired,
        collapsed,
        folded,
    } = &blocks[at]
    else {
        unreachable!()
    };
    assert_eq!(*harness, HarnessId::PI);
    assert!(!*retired);
    assert_eq!(*trigger, Some(CompactTrigger::Manual));
    assert_eq!(
        (*tokens_before, *tokens_after),
        (Some(252_000), Some(52_000))
    );
    assert_eq!(summary.as_deref(), Some("## Goal\nship it"));
    assert_eq!(*kept, KeptAt::Block(user_block(&again, "two")));
    // Folded and collapsed again, as everything comes back.
    assert!(*collapsed && *folded);
    assert_eq!(
        shown_folds(&again),
        before
            .into_iter()
            .map(|f| Fold { folded: true, ..f })
            .collect::<Vec<_>>()
    );
}

/// A turn in a fold can be rewound to: the compaction after it goes with
/// the turns dropped, and the fold with it.
#[test]
fn rewinding_to_a_folded_turn_drops_the_compaction() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    turn(&mut app, "three", "e3");
    app.on_event(compacted(Kept::From("e3".into())));
    let two = user_block(&app, "two");
    assert!(shown_folds(&app)[0].end > two);
    app.take_actions();
    app.rewind_to(two, false);
    assert!(
        !app.controller
            .conversation
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Compaction { .. }))
    );
    assert!(shown_folds(&app).is_empty());
    assert_eq!(app.front.input, "two");
}

/// The fold is the view's: a harness switched to is told the turns in it
/// as any others, and a fork keeps them.
#[test]
fn folded_turns_are_bridged_and_forked_as_any_others() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    assert_eq!(shown_folds(&app).len(), 1);
    app.take_actions();
    app.switch_harness(HarnessId::CLAUDE);
    app.take_actions();
    app.submit_prompt("three".into());
    let sent = sent_turn(&mut app);
    assert!(
        sent.contains("User: one") && sent.contains("User: two"),
        "{sent}"
    );
    app.on_event(done());
    // pi's session is branched: back on it, the copy folds the same.
    app.fork_conversation();
    app.switch_harness(HarnessId::PI);
    assert_eq!(shown_folds(&app).len(), 1);
}

fn current(app: &TestApp) -> Option<usize> {
    current_compaction(
        &app.controller.conversation.transcript.blocks,
        app.controller.active,
    )
}

/// A session pi could not resume, and a fresh one after a rewind pi could
/// not make, know nothing of what the one before compacted: they are told
/// every turn, so nothing is folded, and its summary is what pi knew then.
#[test]
fn a_session_left_holds_nothing_its_compaction_kept() {
    for left in ["replaced", "rewound"] {
        let mut app = test_app(HarnessId::PI);
        turn(&mut app, "one", "e1");
        turn(&mut app, "two", "e2");
        app.on_event(compacted(Kept::From("e2".into())));
        assert_eq!(shown_folds(&app).len(), 1, "{left}");
        assert_eq!(current(&app), Some(compaction_at(&app)), "{left}");
        app.take_actions();
        if left == "replaced" {
            app.on_event(AgentEvent::SessionReplaced {
                reason: "gone".into(),
            });
        } else {
            // A turn whose anchor never came (stopped before pi listed it):
            // no rewind of pi's own reaches it.
            app.submit_prompt("three".into());
            app.on_event(done());
            app.on_event(AgentEvent::TurnAnchor { id: String::new() });
            app.take_actions();
            app.rewind_to(user_block(&app, "three"), false);
        }
        app.take_actions();
        app.submit_prompt("again".into());
        let sent = sent_turn(&mut app);
        assert!(sent.contains("User: one"), "{left}: {sent}");
        assert!(shown_folds(&app).is_empty(), "{left}");
        assert_eq!(current(&app), None, "{left}");
    }
}

/// A fork that starts a harness afresh leaves its session, and what it
/// compacted, behind; one that branches the session keeps them.
#[test]
fn a_fork_that_starts_afresh_leaves_the_compaction_behind() {
    // agy cannot branch its session; the compaction is put on it here.
    let mut app = test_app(HarnessId::AGY);
    turn(&mut app, "one", "a1");
    app.on_event(compacted(Kept::Nothing));
    assert_eq!(shown_folds(&app).len(), 1);
    app.fork_conversation();
    assert!(shown_folds(&app).is_empty());

    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    app.fork_conversation();
    assert_eq!(shown_folds(&app).len(), 1);
}

/// The fold is of the harness in use: one switched to was told the folded
/// turns, and sees them; back on the one that compacted, its session holds
/// the summary again.
#[test]
fn the_fold_is_the_harness_in_uses() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("e2".into())));
    let at = compaction_at(&app);
    app.take_actions();
    app.switch_harness(HarnessId::CLAUDE);
    assert!(shown_folds(&app).is_empty());
    assert_eq!(current(&app), None);
    turn(&mut app, "three", "u3");
    app.take_actions();
    app.switch_harness(HarnessId::PI);
    assert_eq!(shown_folds(&app).len(), 1);
    assert_eq!(current(&app), Some(at));
}

/// A compaction that kept nothing during a turn folds nothing sent into
/// that turn: pi may not have taken a steer yet.
#[test]
fn nothing_kept_does_not_fold_a_steer() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    app.submit_prompt("two".into());
    app.controller.session_alive = true;
    app.steer("and also".into());
    app.on_event(AgentEvent::TextDelta("working".into()));
    app.on_event(compacted(Kept::Nothing));
    let steer = app
        .controller
        .conversation
        .transcript
        .blocks
        .iter()
        .position(|b| matches!(b, Block::User { steer: true, .. }))
        .unwrap();
    assert_eq!(kept(&app), KeptAt::Block(steer));
    // The turns before it: one, and the one the steer went into.
    assert_eq!(shown_folds(&app)[0].turns, 2);
}

/// pi's cut inside a turn is put on that turn once its entries say which
/// it is (`fixtures/compaction_split.jsonl`): the turns before it fold.
#[test]
fn a_cut_inside_a_turn_folds_the_turns_before_it() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "af09e822");
    turn(&mut app, "two", "3898c93b");
    app.on_event(compacted(Kept::From("470f84f8".into())));
    assert_eq!(kept(&app), KeptAt::Unknown);
    app.on_event(AgentEvent::CompactionKept(Kept::InTurnOf(vec![
        "3898c93b".into(),
        "af09e822".into(),
    ])));
    assert_eq!(kept(&app), KeptAt::Block(user_block(&app, "two")));
    assert_eq!(shown_folds(&app)[0].turns, 1);

    // The turn's steer is no turn's anchor: the turn it went into holds it.
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("a2".into())));
    app.on_event(AgentEvent::CompactionKept(Kept::InTurnOf(vec![
        "s2".into(),
        "e2".into(),
        "e1".into(),
    ])));
    assert_eq!(kept(&app), KeptAt::Block(user_block(&app, "two")));

    // During a turn whose anchor is owed: waited for, then put on it.
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    app.submit_prompt("two".into());
    app.on_event(compacted(Kept::From("a2".into())));
    app.on_event(AgentEvent::CompactionKept(Kept::InTurnOf(vec![
        "e2".into(),
        "e1".into(),
    ])));
    assert_eq!(kept(&app), KeptAt::Pending(vec!["e2".into(), "e1".into()]));
    app.on_event(done());
    app.on_event(AgentEvent::TurnAnchor { id: "e2".into() });
    assert_eq!(kept(&app), KeptAt::Block(user_block(&app, "two")));

    // Kept nothing: all before the compaction, also told after it.
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    app.on_event(compacted(Kept::From("c1".into())));
    let at = compaction_at(&app);
    app.on_event(AgentEvent::Notice("later".into()));
    app.on_event(AgentEvent::CompactionKept(Kept::Nothing));
    assert_eq!(kept(&app), KeptAt::Block(at));
}

/// A compaction that kept nothing, before a prompt pi then took: the
/// prompt is in its context, and is not folded, also when its anchor and
/// the entries come after it.
#[test]
fn a_prompt_taken_after_a_compaction_that_kept_nothing_stays() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::From("c".into())));
    app.submit_prompt("three".into());
    app.on_event(AgentEvent::CompactionKept(Kept::InTurnOf(vec![
        "e3".into(),
        "e2".into(),
        "e1".into(),
    ])));
    assert!(matches!(kept(&app), KeptAt::Pending(_)));
    app.on_event(done());
    app.on_event(AgentEvent::TurnAnchor { id: "e3".into() });
    // Its block is after the compaction's: all before the compaction folds.
    assert!(compaction_at(&app) < user_block(&app, "three"));
    assert_eq!(kept(&app), KeptAt::Block(compaction_at(&app)));
    assert_eq!(shown_folds(&app)[0].turns, 2);
}

/// What a harness says of its compaction's kept messages is not taken for
/// a later compaction of another harness's.
#[test]
fn what_a_compaction_kept_is_not_taken_for_another_harnesss() {
    let mut app = test_app(HarnessId::PI);
    turn(&mut app, "one", "e1");
    turn(&mut app, "two", "e2");
    app.on_event(compacted(Kept::Unknown));
    let pi = compaction_at(&app);
    app.take_actions();
    app.switch_harness(HarnessId::CLAUDE);
    turn(&mut app, "three", "u3");
    app.on_event(compacted(Kept::Unknown));
    app.take_actions();
    app.switch_harness(HarnessId::PI);
    app.on_event(AgentEvent::CompactionKept(Kept::InTurnOf(vec![
        "e2".into(),
    ])));
    assert!(matches!(
        &app.controller.conversation.transcript.blocks[pi],
        Block::Compaction {
            kept: KeptAt::Unknown,
            ..
        }
    ));
}
