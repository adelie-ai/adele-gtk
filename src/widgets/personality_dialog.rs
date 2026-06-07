//! Per-conversation personality picker (#70). A modal dialog with one row per
//! "Expressive 7" trait. Each row is a `DropDown` offering **Global** (inherit
//! the daemon's global disposition) plus the five concrete levels
//! (Never/Rarely/Sometimes/Often/Always).
//!
//! Unlike the header model picker (a live per-send override), the personality
//! override is persisted on the daemon: Save calls
//! `set_conversation_personality` through the async bridge; the daemon stores
//! the partial override and resolves each `None` trait against the global config
//! on every send. An all-**Global** selection clears the override.
//!
//! The dropdown index ↔ override mapping is pure and unit-tested below so the
//! "Global = None" / "level = Some" contract can't drift without a failing test.

use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, DropDown, Label, Orientation, StringList, Window, glib};

/// The "Expressive 7" traits, in the order the wire contract (#236) lists
/// them: professionalism, warmth, directness, enthusiasm, humor, sarcasm,
/// pretentiousness. The label is the user-facing row title.
const TRAITS: [&str; 7] = [
    "Professionalism",
    "Warmth",
    "Directness",
    "Enthusiasm",
    "Humor",
    "Sarcasm",
    "Pretentiousness",
];

/// The per-row dropdown entries. Index 0 is the "inherit global" sentinel; the
/// remaining five are the concrete [`api::PersonalityLevel`] variants in
/// ascending order.
const ROW_OPTIONS: [&str; 6] = ["Global", "Never", "Rarely", "Sometimes", "Often", "Always"];

/// Map a dropdown row index to a trait override value. Index 0 ("Global") is
/// `None` (inherit); 1..=5 map to `Never`..=`Always`. Out-of-range indices fall
/// back to `None` (inherit) rather than panicking, so a malformed selection
/// degrades to the safe "inherit global" behaviour.
fn level_from_index(index: u32) -> Option<api::PersonalityLevel> {
    match index {
        1 => Some(api::PersonalityLevel::Never),
        2 => Some(api::PersonalityLevel::Rarely),
        3 => Some(api::PersonalityLevel::Sometimes),
        4 => Some(api::PersonalityLevel::Often),
        5 => Some(api::PersonalityLevel::Always),
        // 0 ("Global") and anything out of range → inherit.
        _ => None,
    }
}

/// Inverse of [`level_from_index`]: map a trait override value to its dropdown
/// row index. `None` ("inherit global") is index 0.
fn index_from_level(level: Option<api::PersonalityLevel>) -> u32 {
    match level {
        None => 0,
        Some(api::PersonalityLevel::Never) => 1,
        Some(api::PersonalityLevel::Rarely) => 2,
        Some(api::PersonalityLevel::Sometimes) => 3,
        Some(api::PersonalityLevel::Often) => 4,
        Some(api::PersonalityLevel::Always) => 5,
    }
}

/// Build a [`api::ConversationPersonalityView`] from the 7 dropdown indices, in
/// the canonical trait order. All-`Global` (every index 0) yields the all-`None`
/// override, which the daemon treats as "cleared".
fn override_from_indices(indices: [u32; 7]) -> api::ConversationPersonalityView {
    api::ConversationPersonalityView {
        professionalism: level_from_index(indices[0]),
        warmth: level_from_index(indices[1]),
        directness: level_from_index(indices[2]),
        enthusiasm: level_from_index(indices[3]),
        humor: level_from_index(indices[4]),
        sarcasm: level_from_index(indices[5]),
        pretentiousness: level_from_index(indices[6]),
    }
}

/// Inverse of [`override_from_indices`]: derive the 7 dropdown indices from a
/// stored override so the dialog pre-fills. A missing override (`None`) pre-fills
/// every row to "Global" (index 0).
fn indices_from_override(over: Option<&api::ConversationPersonalityView>) -> [u32; 7] {
    let over = match over {
        Some(o) => o,
        None => return [0; 7],
    };
    [
        index_from_level(over.professionalism),
        index_from_level(over.warmth),
        index_from_level(over.directness),
        index_from_level(over.enthusiasm),
        index_from_level(over.humor),
        index_from_level(over.sarcasm),
        index_from_level(over.pretentiousness),
    ]
}

/// Show the per-conversation personality picker as a modal dialog.
///
/// `prefill` is the conversation's stored override (from
/// `ConversationDetail::conversation_personality`); each trait pre-selects
/// "Global" when its value is `None`. On Save, `on_save` is invoked with the
/// assembled [`api::ConversationPersonalityView`] (all-`None` = clear); Cancel
/// discards without calling it.
pub fn show_personality_dialog<F>(
    parent: &impl IsA<Window>,
    prefill: Option<&api::ConversationPersonalityView>,
    on_save: F,
) where
    F: Fn(api::ConversationPersonalityView) + 'static,
{
    let dialog = Window::builder()
        .title("Conversation Personality")
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 0);

    let intro = Label::new(Some(
        "Tune this conversation's personality. Each trait set to \
         \"Global\" inherits your global setting; pin a level to override it \
         for this conversation only.",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.set_margin_start(16);
    intro.set_margin_end(16);
    intro.set_margin_top(16);
    intro.set_margin_bottom(12);
    outer.append(&intro);

    let prefill_indices = indices_from_override(prefill);

    // One row per trait: a label on the left, a dropdown on the right. The
    // dropdowns are collected so Save can read every selection at once.
    let dropdowns: Rc<Vec<DropDown>> = Rc::new(
        TRAITS
            .iter()
            .enumerate()
            .map(|(i, trait_name)| {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                row.set_margin_start(16);
                row.set_margin_end(16);
                row.set_margin_top(4);
                row.set_margin_bottom(4);

                let label = Label::new(Some(trait_name));
                label.set_halign(Align::Start);
                label.set_hexpand(true);
                row.append(&label);

                let options = StringList::new(&ROW_OPTIONS);
                let dd = DropDown::new(Some(options), gtk4::Expression::NONE);
                dd.set_selected(prefill_indices[i]);
                row.append(&dd);

                outer.append(&row);
                dd
            })
            .collect(),
    );

    let btn_row = GtkBox::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    btn_row.set_margin_top(12);
    btn_row.set_margin_bottom(16);
    btn_row.set_margin_start(16);
    btn_row.set_margin_end(16);

    let cancel = Button::with_label("Cancel");
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    btn_row.append(&cancel);

    let save = Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.connect_clicked(glib::clone!(
        #[weak(rename_to = dialog_ref)]
        dialog,
        #[strong]
        dropdowns,
        move |_| {
            let mut indices = [0u32; 7];
            for (i, dd) in dropdowns.iter().enumerate() {
                indices[i] = dd.selected();
            }
            let personality = override_from_indices(indices);
            dialog_ref.close();
            on_save(personality);
        }
    ));
    btn_row.append(&save);

    outer.append(&btn_row);

    dialog.set_child(Some(&outer));
    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_index_maps_to_none_and_back() {
        assert_eq!(level_from_index(0), None);
        assert_eq!(index_from_level(None), 0);
    }

    #[test]
    fn every_level_round_trips_through_its_index() {
        let levels = [
            api::PersonalityLevel::Never,
            api::PersonalityLevel::Rarely,
            api::PersonalityLevel::Sometimes,
            api::PersonalityLevel::Often,
            api::PersonalityLevel::Always,
        ];
        for level in levels {
            let idx = index_from_level(Some(level));
            assert_eq!(level_from_index(idx), Some(level), "round-trip {level:?}");
        }
        // The five concrete levels occupy indices 1..=5.
        assert_eq!(index_from_level(Some(api::PersonalityLevel::Never)), 1);
        assert_eq!(index_from_level(Some(api::PersonalityLevel::Always)), 5);
    }

    #[test]
    fn out_of_range_index_falls_back_to_inherit() {
        // A malformed selection must degrade to "inherit global", never panic.
        assert_eq!(level_from_index(6), None);
        assert_eq!(level_from_index(u32::MAX), None);
    }

    #[test]
    fn all_global_indices_produce_cleared_override() {
        let over = override_from_indices([0; 7]);
        assert_eq!(over, api::ConversationPersonalityView::default());
        // Default is all-`None`, which the daemon treats as "cleared".
        assert!(over.professionalism.is_none());
        assert!(over.pretentiousness.is_none());
    }

    #[test]
    fn indices_map_to_the_canonical_trait_order() {
        // Distinct level per slot proves no two traits are swapped: assign
        // index = position+1 so each trait gets a different level.
        let over = override_from_indices([1, 2, 3, 4, 5, 1, 2]);
        assert_eq!(over.professionalism, Some(api::PersonalityLevel::Never));
        assert_eq!(over.warmth, Some(api::PersonalityLevel::Rarely));
        assert_eq!(over.directness, Some(api::PersonalityLevel::Sometimes));
        assert_eq!(over.enthusiasm, Some(api::PersonalityLevel::Often));
        assert_eq!(over.humor, Some(api::PersonalityLevel::Always));
        assert_eq!(over.sarcasm, Some(api::PersonalityLevel::Never));
        assert_eq!(over.pretentiousness, Some(api::PersonalityLevel::Rarely));
    }

    #[test]
    fn prefill_none_selects_global_for_every_row() {
        assert_eq!(indices_from_override(None), [0; 7]);
    }

    #[test]
    fn prefill_round_trips_a_partial_override() {
        // Humor=Never, directness=Always, rest inherited — the issue's
        // acceptance example.
        let stored = api::ConversationPersonalityView {
            humor: Some(api::PersonalityLevel::Never),
            directness: Some(api::PersonalityLevel::Always),
            ..api::ConversationPersonalityView::default()
        };
        let indices = indices_from_override(Some(&stored));
        // Re-assembling from the pre-filled indices yields the same override.
        assert_eq!(override_from_indices(indices), stored);
        // Spot-check the two pinned slots land on the right rows.
        assert_eq!(indices[2], 5, "directness → Always");
        assert_eq!(indices[4], 1, "humor → Never");
    }

    #[test]
    fn row_options_have_global_plus_five_levels() {
        // The dropdown must offer exactly Global + the 5 levels, Global first.
        assert_eq!(ROW_OPTIONS.len(), 6);
        assert_eq!(ROW_OPTIONS[0], "Global");
        // Each non-Global option maps to a distinct concrete level.
        for idx in 1..ROW_OPTIONS.len() as u32 {
            assert!(level_from_index(idx).is_some(), "option {idx} is a level");
        }
    }
}
