//! Transcript entries, and the turn-identity decisions the chat view makes.
//!
//! Deliberately free of GTK types. The two questions the transcript's
//! right-click menu asks - which entry is the pointer over, and does that turn
//! have an id to copy - are answered here, so they can be tested without a GTK
//! main loop.
//!
//! A turn's identity is the client-minted idempotency key the daemon persists
//! on the turn's USER message (`messages.idempotency_key`, daemon migration
//! 034). That column is nullable, and three ordinary cases arrive without one:
//! a message stored before the migration, a message another client sent, and a
//! send over a transport that drops the key (the legacy D-Bus conversation API
//! returns `(role, content)` only). So "no id" is a normal state, not a fault,
//! and the menu must never present an empty id as a copied one.

use desktop_assistant_client_common::MessageKind;

/// One line of the rendered transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// `user`, `assistant`, or another daemon role.
    pub role: String,
    pub content: String,
    /// Presentation metadata (voice#126), never parsed out of `content`.
    pub kind: MessageKind,
    /// The turn identity. Present only on a user message that carries one.
    pub turn_id: Option<String>,
}

impl TranscriptEntry {
    /// A daemon-sourced or locally-drawn line.
    pub fn new(
        role: impl Into<String>,
        content: impl Into<String>,
        kind: MessageKind,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            kind,
            turn_id,
        }
    }

    /// Whether this entry is the user message that starts a turn.
    pub fn is_user(&self) -> bool {
        self.role == "user"
    }
}

/// What the transcript context menu can do with the turn under the pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnIdAction {
    /// The turn has an id. This exact text is what goes on the clipboard.
    Copy(String),
    /// The turn has no id. The entry is shown but cannot be chosen, so a
    /// person sees that the option exists and that it is not available here.
    /// Carrying no text at all is what makes an empty clipboard write
    /// impossible rather than merely unlikely.
    Unavailable,
}

/// The turn-id action for the transcript entry at `index`.
///
/// A turn starts at its user message; the reply and any client-local line that
/// follows belong to the same turn, so a right-click anywhere in the turn
/// resolves to the same id. The walk back stops at the nearest user message: an
/// older turn's id is not this turn's id, and offering it would copy an id that
/// points at the wrong turn - a worse failure than offering none.
pub fn turn_id_action(entries: &[TranscriptEntry], index: usize) -> TurnIdAction {
    let Some(window) = entries.get(..=index) else {
        return TurnIdAction::Unavailable;
    };
    let Some(start) = window.iter().rposition(TranscriptEntry::is_user) else {
        return TurnIdAction::Unavailable;
    };
    // Trimmed because what reaches the clipboard has to paste as it stands: a
    // stored id padded with whitespace would otherwise carry invisible
    // characters into `adele inspect turn`. An id that is only whitespace is
    // no id at all.
    match entries[start].turn_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => TurnIdAction::Copy(id.to_string()),
        _ => TurnIdAction::Unavailable,
    }
}

/// The pointer-resolved context of one right-click in the transcript.
#[cfg(feature = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptClick {
    /// The transcript entry under the pointer, or `None` when the pointer was
    /// over the page background rather than a message.
    pub entry_index: Option<usize>,
    /// The text the page has selected, empty when there is none.
    pub selection: String,
}

/// Parse the reply of the transcript hit-test script: the entry index on the
/// first line, then the page selection, which may itself contain newlines.
///
/// Two values in one reply because one round trip to the web process is one
/// chance for the page to change under us; two would let the index and the
/// selection describe different moments.
#[cfg(feature = "linux")]
pub fn parse_transcript_click(raw: &str) -> TranscriptClick {
    let (head, selection) = raw.split_once('\n').unwrap_or((raw, ""));
    TranscriptClick {
        // Anything the page could not answer - `-1` for the background, an
        // empty reply from a page that is still loading - parses to no entry.
        entry_index: head.trim().parse::<usize>().ok(),
        selection: selection.to_string(),
    }
}

/// The transcript entry containing buffer `offset`, given each entry's start
/// offset in ascending order.
///
/// The `TextView` fallback has no DOM to hit-test, so it records where each
/// entry begins as it renders and resolves a click position against those
/// marks instead.
#[cfg(not(feature = "linux"))]
pub fn entry_index_at_offset(starts: &[i32], offset: i32) -> Option<usize> {
    starts.iter().rposition(|start| *start <= offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str, turn_id: Option<&str>) -> TranscriptEntry {
        TranscriptEntry::new(
            "user",
            content,
            MessageKind::Normal,
            turn_id.map(str::to_string),
        )
    }

    fn assistant(content: &str) -> TranscriptEntry {
        TranscriptEntry::new("assistant", content, MessageKind::Normal, None)
    }

    // --- gtk#169 item 1, acceptance criterion: Copy turn id puts the id on
    // the clipboard --------------------------------------------------------

    #[test]
    fn right_clicking_the_user_message_offers_that_turns_id() {
        let entries = vec![user("why?", Some("turn-a")), assistant("because")];
        assert_eq!(
            turn_id_action(&entries, 0),
            TurnIdAction::Copy("turn-a".to_string())
        );
    }

    #[test]
    fn right_clicking_the_reply_offers_the_id_of_the_turn_it_belongs_to() {
        let entries = vec![user("why?", Some("turn-a")), assistant("because")];
        assert_eq!(
            turn_id_action(&entries, 1),
            TurnIdAction::Copy("turn-a".to_string())
        );
    }

    #[test]
    fn a_client_local_line_after_the_reply_still_resolves_to_the_same_turn() {
        // A `say_this` aside (voice#126) is drawn in the assistant column and
        // belongs to the turn that produced it.
        let entries = vec![
            user("why?", Some("turn-a")),
            assistant("because"),
            TranscriptEntry::new("assistant", "spoken aside", MessageKind::Spoken, None),
        ];
        assert_eq!(
            turn_id_action(&entries, 2),
            TurnIdAction::Copy("turn-a".to_string())
        );
    }

    // --- gtk#169 item 1, acceptance criterion: the item is unavailable for a
    // turn that has no id --------------------------------------------------

    #[test]
    fn a_turn_whose_user_message_has_no_id_is_unavailable() {
        let entries = vec![user("why?", None), assistant("because")];
        assert_eq!(turn_id_action(&entries, 0), TurnIdAction::Unavailable);
        assert_eq!(turn_id_action(&entries, 1), TurnIdAction::Unavailable);
    }

    #[test]
    fn an_empty_id_is_never_offered_as_a_copyable_one() {
        // The one failure mode this item has: writing "" to the clipboard and
        // reporting success.
        for stored in ["", "   ", "\t\n"] {
            let entries = vec![user("why?", Some(stored))];
            assert_eq!(
                turn_id_action(&entries, 0),
                TurnIdAction::Unavailable,
                "a blank stored id ({stored:?}) must not be offered"
            );
        }
    }

    #[test]
    fn an_offered_id_is_never_blank() {
        // The property behind the case above, stated over the whole decision:
        // whatever `Copy` carries is something a person can paste.
        let entries = vec![
            user("a", Some("turn-a")),
            assistant("x"),
            user("b", None),
            assistant("y"),
            user("c", Some("  turn-c  ")),
        ];
        for index in 0..entries.len() {
            if let TurnIdAction::Copy(id) = turn_id_action(&entries, index) {
                assert!(
                    !id.trim().is_empty(),
                    "entry {index} offered a blank id: {id:?}"
                );
            }
        }
    }

    #[test]
    fn a_padded_id_is_offered_trimmed() {
        let entries = vec![user("why?", Some("  turn-a\n"))];
        assert_eq!(
            turn_id_action(&entries, 0),
            TurnIdAction::Copy("turn-a".to_string())
        );
    }

    #[test]
    fn a_later_turn_does_not_borrow_the_previous_turns_id() {
        // The walk back must stop at the nearest user message. A turn sent by
        // another client arrives with no key; its reply must not be labelled
        // with the id of the turn before it.
        let entries = vec![
            user("first", Some("turn-a")),
            assistant("reply one"),
            user("second", None),
            assistant("reply two"),
        ];
        assert_eq!(turn_id_action(&entries, 2), TurnIdAction::Unavailable);
        assert_eq!(turn_id_action(&entries, 3), TurnIdAction::Unavailable);
    }

    #[test]
    fn a_line_before_any_user_message_is_unavailable() {
        let entries = vec![assistant("greeting"), user("hi", Some("turn-a"))];
        assert_eq!(turn_id_action(&entries, 0), TurnIdAction::Unavailable);
    }

    #[test]
    fn an_index_past_the_transcript_is_unavailable() {
        let entries = vec![user("why?", Some("turn-a"))];
        assert_eq!(turn_id_action(&entries, 1), TurnIdAction::Unavailable);
        assert_eq!(
            turn_id_action(&entries, usize::MAX),
            TurnIdAction::Unavailable
        );
        assert_eq!(turn_id_action(&[], 0), TurnIdAction::Unavailable);
    }

    // --- resolving the pointer to an entry ---------------------------------

    #[cfg(feature = "linux")]
    #[test]
    fn a_hit_test_reply_carries_the_entry_index_and_the_selection() {
        let click = parse_transcript_click("2\nselected words");
        assert_eq!(click.entry_index, Some(2));
        assert_eq!(click.selection, "selected words");
    }

    #[cfg(feature = "linux")]
    #[test]
    fn a_multi_line_selection_survives_the_reply() {
        let click = parse_transcript_click("0\nline one\nline two\n");
        assert_eq!(click.entry_index, Some(0));
        assert_eq!(click.selection, "line one\nline two\n");
    }

    #[cfg(feature = "linux")]
    #[test]
    fn a_click_on_the_page_background_resolves_to_no_entry() {
        // The script reports -1 when the pointer was not over a message.
        let click = parse_transcript_click("-1\n");
        assert_eq!(click.entry_index, None);
        assert!(click.selection.is_empty());
    }

    #[cfg(feature = "linux")]
    #[test]
    fn an_unreadable_reply_resolves_to_no_entry() {
        // A reply the page could not produce must degrade to "no turn here",
        // never to entry zero.
        for raw in ["", "null", "undefined", "not-a-number\nx"] {
            assert_eq!(
                parse_transcript_click(raw).entry_index,
                None,
                "reply {raw:?} must not resolve to an entry"
            );
        }
    }

    #[cfg(not(feature = "linux"))]
    #[test]
    fn a_buffer_offset_resolves_to_the_entry_that_contains_it() {
        let starts = [0, 40, 90];
        assert_eq!(entry_index_at_offset(&starts, 0), Some(0));
        assert_eq!(entry_index_at_offset(&starts, 39), Some(0));
        assert_eq!(entry_index_at_offset(&starts, 40), Some(1));
        assert_eq!(entry_index_at_offset(&starts, 89), Some(1));
        assert_eq!(entry_index_at_offset(&starts, 900), Some(2));
    }

    #[cfg(not(feature = "linux"))]
    #[test]
    fn an_offset_before_the_first_entry_resolves_to_none() {
        assert_eq!(entry_index_at_offset(&[10, 40], 3), None);
        assert_eq!(entry_index_at_offset(&[], 0), None);
    }
}
