//! Shared right-click context-menu builder for list rows.
//!
//! The sidebar (conversation rows) and the login screen (connection-profile
//! rows) both pop a small `Popover` of action buttons on secondary click. The
//! scaffolding is identical — parent the popover to the clicked widget, unparent
//! it on close so it can't leak or warn "finalized while parented" (GTK-5),
//! point it at the cursor, stack labelled buttons, and pop down before running
//! each action. Only the button set differs, so that is the one thing callers
//! supply. Centralising the popover lifecycle here means the GTK-5 fix lives in
//! exactly one place.

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Orientation, Popover, Widget, glib};

/// One entry in a context menu: its label, whether it is a destructive action
/// (styled red), and the callback to run when chosen. The callback runs *after*
/// the popover has popped down.
pub struct MenuItem {
    pub label: String,
    pub destructive: bool,
    pub action: Box<dyn Fn()>,
}

impl MenuItem {
    /// A normal (non-destructive) menu entry.
    pub fn new(label: impl Into<String>, action: impl Fn() + 'static) -> Self {
        Self {
            label: label.into(),
            destructive: false,
            action: Box::new(action),
        }
    }

    /// A destructive menu entry (e.g. Delete), styled with
    /// `destructive-action`.
    pub fn destructive(label: impl Into<String>, action: impl Fn() + 'static) -> Self {
        Self {
            label: label.into(),
            destructive: true,
            action: Box::new(action),
        }
    }
}

/// Build and show a context-menu popover of `items`, parented to `widget` and
/// pointing at `(x, y)` (the cursor position from the gesture, in `widget`
/// coordinates).
///
/// The popover unparents itself on close (GTK-5) so it is released rather than
/// leaking attached to the row. Each button pops the menu down and then runs its
/// action.
pub fn show(widget: &Widget, x: f64, y: f64, items: Vec<MenuItem>) {
    let popover = Popover::new();
    popover.add_css_class("context-popover");
    popover.set_parent(widget);
    // Unparent on close so the popover (and its widget tree) is released instead
    // of leaking parented to the row, which also raised "finalized while
    // parented" warnings on every repaint (GTK-5).
    popover.connect_closed(|p| p.unparent());
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.set_has_arrow(false);

    let menu_box = GtkBox::new(Orientation::Vertical, 0);
    for item in items {
        let button = Button::with_label(&item.label);
        button.add_css_class("context-button");
        if item.destructive {
            button.add_css_class("destructive-action");
        }
        let action = item.action;
        button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                action();
            }
        ));
        menu_box.append(&button);
    }

    popover.set_child(Some(&menu_box));
    popover.popup();
}
