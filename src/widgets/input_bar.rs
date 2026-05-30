use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Orientation, ScrolledWindow, TextView, WrapMode};

/// Input bar widget with a text view and send button.
pub struct InputBar {
    pub container: GtkBox,
    pub text_view: TextView,
    pub send_button: Button,
}

impl InputBar {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 8);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(8);

        let scrolled = ScrolledWindow::new();
        scrolled.set_hexpand(true);
        scrolled.set_max_content_height(100); // ~4 lines
        scrolled.set_propagate_natural_height(true);

        let text_view = TextView::new();
        text_view.set_wrap_mode(WrapMode::WordChar);
        text_view.set_top_margin(8);
        text_view.set_bottom_margin(8);
        text_view.set_left_margin(12);
        text_view.set_right_margin(12);
        text_view.add_css_class("input-textview");
        scrolled.set_child(Some(&text_view));
        container.append(&scrolled);

        let send_button = Button::with_label("Send");
        send_button.add_css_class("send-button");
        send_button.set_valign(gtk4::Align::End);
        send_button.set_margin_bottom(4);
        container.append(&send_button);

        Self {
            container,
            text_view,
            send_button,
        }
    }

    /// Get the current text content and clear the input.
    pub fn take_text(&self) -> String {
        let buffer = self.text_view.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        buffer.set_text("");
        text
    }

    /// Get the current text content without clearing.
    // Read-only counterpart to `take_text` (which is used). Part of the public
    // InputBar API; not yet called from `window.rs` but kept for the input-bar
    // consumers added by the connections/model-selector work (#1).
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        let buffer = self.text_view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }
}
