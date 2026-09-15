//! Parity tests for `Editor::handle_input` (pi v0.84.3
//! `components/editor.ts` `handleInput`): the keybinding dispatch, paste
//! assembly, jump mode, and the reported `onChange` / `onSubmit` events.
//!
//! The autocomplete provider stays host-side, so the Tab / Enter completion
//! paths only close the menu (documented divergence).

use pillar_tui::editor::{Editor, EditorInputEvent};
use pillar_tui::editor_autocomplete::create_autocomplete_list;

/// Type a string one character at a time and return the events of the last
/// call.
fn type_text(editor: &mut Editor, text: &str) {
    for ch in text.chars() {
        editor.handle_input(&ch.to_string());
        editor.take_input_events();
    }
}

fn editor_with(text: &str) -> Editor {
    let mut editor = Editor::new();
    type_text(&mut editor, text);
    editor
}

#[test]
fn typing_inserts_and_reports_a_single_change() {
    let mut editor = Editor::new();
    editor.take_input_events();
    editor.handle_input("a");
    assert_eq!(editor.get_text(), "a");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
    // A key that changes nothing reports nothing.
    editor.handle_input("\u{1b}[C"); // right arrow
    assert!(editor.take_input_events().is_empty());
}

#[test]
fn enter_submits_the_trimmed_text_and_clears_the_editor() {
    let mut editor = editor_with("  hello  ");
    editor.take_input_events();
    editor.handle_input("\r");
    assert_eq!(
        editor.take_input_events(),
        vec![
            EditorInputEvent::Changed,
            EditorInputEvent::Submitted("hello".to_string())
        ]
    );
    assert_eq!(editor.get_text(), "");
}

#[test]
fn enter_with_a_trailing_backslash_inserts_a_newline_instead() {
    let mut editor = editor_with("hi\\");
    editor.take_input_events();
    editor.handle_input("\r");
    assert_eq!(editor.get_text(), "hi\n");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
}

#[test]
fn shift_enter_newlines_without_submitting() {
    let mut editor = editor_with("hi");
    editor.take_input_events();
    // ctrl+j is one of the newLine bindings; "\n" alone is the legacy form.
    editor.handle_input("\n");
    assert_eq!(editor.get_text(), "hi\n");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
}

#[test]
fn disable_submit_swallows_enter() {
    let mut editor = editor_with("hi");
    editor.disable_submit = true;
    editor.take_input_events();
    editor.handle_input("\r");
    assert_eq!(editor.get_text(), "hi");
    assert!(editor.take_input_events().is_empty());
}

#[test]
fn deletion_bindings_edit_the_line() {
    let mut editor = editor_with("hello world");
    editor.take_input_events();
    editor.handle_input("\u{7f}"); // backspace
    assert_eq!(editor.get_text(), "hello worl");
    editor.handle_input("\u{17}"); // ctrl+w - delete word backward
    assert_eq!(editor.get_text(), "hello ");
    editor.handle_input("\u{15}"); // ctrl+u - delete to line start
    assert_eq!(editor.get_text(), "");
    assert_eq!(editor.take_input_events().len(), 3);
}

#[test]
fn ctrl_c_is_consumed_without_editor_changes() {
    let mut editor = editor_with("hello");
    editor.take_input_events();
    editor.handle_input("\u{3}");
    assert_eq!(editor.get_text(), "hello");
    assert!(editor.take_input_events().is_empty());
}

#[test]
fn escape_is_not_editor_input() {
    let mut editor = editor_with("hello");
    editor.take_input_events();
    editor.handle_input("\u{1b}");
    assert_eq!(editor.get_text(), "hello");
    assert!(editor.take_input_events().is_empty());
}

#[test]
fn up_arrow_navigates_prompt_history_when_idle() {
    let mut editor = Editor::new();
    editor.add_to_history("first");
    editor.take_input_events();
    editor.handle_input("\u{1b}[A"); // up
    assert_eq!(editor.get_text(), "first");
    assert_eq!(editor.history_index(), 0);
    editor.handle_input("\u{1b}[B"); // down
    assert_eq!(editor.get_text(), "");
    assert_eq!(editor.history_index(), -1);
}

#[test]
fn single_sequence_paste_is_inserted() {
    let mut editor = Editor::new();
    editor.take_input_events();
    editor.handle_input("\u{1b}[200~hello\u{1b}[201~");
    assert_eq!(editor.get_text(), "hello");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
}

#[test]
fn paste_split_across_calls_is_buffered_until_the_end_marker() {
    let mut editor = Editor::new();
    editor.take_input_events();
    editor.handle_input("\u{1b}[200~hel");
    assert_eq!(editor.get_text(), "");
    assert!(editor.take_input_events().is_empty());
    editor.handle_input("lo\u{1b}[201~");
    assert_eq!(editor.get_text(), "hello");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
}

#[test]
fn undo_restores_the_previous_text() {
    let mut editor = editor_with("hello");
    editor.take_input_events();
    editor.handle_input("\u{1f}"); // ctrl+- - undo
    assert_eq!(editor.get_text(), "");
    assert_eq!(editor.take_input_events(), vec![EditorInputEvent::Changed]);
}

#[test]
fn jump_mode_waits_for_the_target_character() {
    let mut editor = editor_with("a-b-c");
    editor.move_to_line_start();
    editor.take_input_events();
    editor.handle_input("\u{1d}"); // ctrl+] - jump forward
    assert_eq!(editor.get_text(), "a-b-c");
    assert!(editor.take_input_events().is_empty());
    editor.handle_input("-");
    assert_eq!(editor.get_cursor(), (0, 1));
    assert!(editor.take_input_events().is_empty());
}

#[test]
fn page_keys_scroll_without_changing_the_text() {
    let mut editor = editor_with(&"line\n".repeat(20));
    editor.take_input_events();
    editor.handle_input("\u{1b}[5~"); // page up
    // Page up only moves the cursor, so nothing is reported.
    assert!(editor.take_input_events().is_empty());
    assert_eq!(editor.get_text(), "line\n".repeat(20));
}

#[test]
fn autocomplete_menu_navigates_and_closes() {
    let items = vec![
        ("a".to_string(), "alpha".to_string(), None),
        ("b".to_string(), "beta".to_string(), None),
    ];
    let mut editor = editor_with("/");
    editor.set_autocomplete_list(Some(create_autocomplete_list("/", &items, 5)));
    assert!(editor.is_showing_autocomplete());
    editor.take_input_events();
    editor.handle_input("\u{1b}[B"); // down
    assert!(editor.is_showing_autocomplete());
    editor.handle_input("\u{1b}"); // escape closes the menu (select.cancel)
    assert!(!editor.is_showing_autocomplete());
    assert!(editor.take_input_events().is_empty());
    assert_eq!(editor.get_text(), "/");
}
