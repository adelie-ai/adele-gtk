//! Markdown → GTK `TextBuffer` rendering for the non-WebView build
//! (`--no-default-features`, i.e. platforms without WebKitGTK such as macOS).
//!
//! The `linux`/WebView path renders assistant markdown to sanitized HTML (see
//! [`crate::markdown`]); this module drives the *same* `pulldown-cmark` parser
//! but emits `TextView` tags instead, so the fallback pane shows formatted text
//! (bold/italic/inline code/code blocks/headings/lists/links) rather than the
//! old flat single-`Label` string.
//!
//! Scope mirrors the parser's event stream, not a full HTML engine: there is no
//! table layout or image rendering (a fallback doesn't need them). Raw embedded
//! HTML is dropped rather than shown — the WebView path sanitizes it with
//! `ammonia`; here there is nothing to render it into.

use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{TextBuffer, TextTag};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Text tags installed once on a buffer's tag table and reused across renders.
///
/// One set is created per [`crate::widgets::chat_view::ChatView`] (each owns its
/// own buffer, hence its own tag table), so the tag names never collide.
pub struct MarkdownTags {
    bold: TextTag,
    italic: TextTag,
    strikethrough: TextTag,
    code: TextTag,
    code_block: TextTag,
    heading: TextTag,
    link: TextTag,
    /// The "You" / "Adele" speaker header that precedes each message.
    role: TextTag,
}

impl MarkdownTags {
    /// Create and register the tags on `buffer`'s tag table.
    pub fn install(buffer: &TextBuffer) -> Self {
        let bold = buffer
            .create_tag(Some("md-bold"), &[])
            .expect("create md-bold");
        bold.set_weight(700);

        let italic = buffer
            .create_tag(Some("md-italic"), &[])
            .expect("create md-italic");
        italic.set_style(pango::Style::Italic);

        let strikethrough = buffer
            .create_tag(Some("md-strike"), &[])
            .expect("create md-strike");
        strikethrough.set_strikethrough(true);

        let code = buffer
            .create_tag(Some("md-code"), &[])
            .expect("create md-code");
        code.set_family(Some("monospace"));

        let code_block = buffer
            .create_tag(Some("md-codeblock"), &[])
            .expect("create md-codeblock");
        code_block.set_family(Some("monospace"));
        code_block.set_left_margin(24);
        code_block.set_pixels_above_lines(4);
        code_block.set_pixels_below_lines(4);

        let heading = buffer
            .create_tag(Some("md-heading"), &[])
            .expect("create md-heading");
        heading.set_weight(700);
        heading.set_scale(1.3);

        let link = buffer
            .create_tag(Some("md-link"), &[])
            .expect("create md-link");
        link.set_underline(pango::Underline::Single);
        link.set_foreground(Some("#4ea1ff"));

        let role = buffer
            .create_tag(Some("md-role"), &[])
            .expect("create md-role");
        role.set_weight(700);
        role.set_scale(1.05);
        role.set_pixels_above_lines(10);
        role.set_pixels_below_lines(2);

        Self {
            bold,
            italic,
            strikethrough,
            code,
            code_block,
            heading,
            link,
            role,
        }
    }

    /// Insert a speaker header ("You" / "Adele") at the end of the buffer.
    pub fn insert_role(&self, buffer: &TextBuffer, label: &str) {
        let mut end = buffer.end_iter();
        buffer.insert_with_tags(&mut end, &format!("{label}\n"), &[&self.role]);
    }
}

/// Append `markdown`, rendered with `tags`, at the end of `buffer`.
///
/// Partial/streaming input is fine: `pulldown-cmark` treats an unterminated
/// `**`/`` ` `` run as literal text, so a mid-stream re-render never panics or
/// leaves a tag "stuck" open across messages (the active-tag stack is local).
pub fn render(buffer: &TextBuffer, tags: &MarkdownTags, markdown: &str) {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    // Inline/styled tags currently in scope, innermost last. Block nesting in
    // markdown is well-formed, so push-on-Start / pop-on-End stays balanced.
    let mut active: Vec<TextTag> = Vec::new();
    // List nesting: `Some(next_number)` for an ordered list, `None` for a
    // bulleted one. Depth = stack length, used to indent nested items.
    let mut lists: Vec<Option<u64>> = Vec::new();

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Strong => active.push(tags.bold.clone()),
                Tag::Emphasis => active.push(tags.italic.clone()),
                Tag::Strikethrough => active.push(tags.strikethrough.clone()),
                Tag::Heading { .. } => active.push(tags.heading.clone()),
                Tag::CodeBlock(_) => active.push(tags.code_block.clone()),
                Tag::Link { .. } => active.push(tags.link.clone()),
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    let indent = "    ".repeat(lists.len().saturating_sub(1));
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{indent}{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => format!("{indent}\u{2022} "),
                    };
                    insert(buffer, &[], &marker);
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => {
                    active.pop();
                }
                TagEnd::Heading(_) => {
                    active.pop();
                    insert(buffer, &[], "\n\n");
                }
                TagEnd::CodeBlock => {
                    active.pop();
                    insert(buffer, &[], "\n");
                }
                TagEnd::Paragraph => insert(buffer, &[], "\n\n"),
                TagEnd::Item => insert(buffer, &[], "\n"),
                TagEnd::List(_) => {
                    lists.pop();
                }
                _ => {}
            },
            Event::Text(text) => insert(buffer, &active, &text),
            Event::Code(text) => {
                let mut inline = active.clone();
                inline.push(tags.code.clone());
                insert(buffer, &inline, &text);
            }
            // A single newline in markdown is a soft break, rendered as a space
            // (matching the WebView/HTML path, where HTML collapses it).
            Event::SoftBreak => insert(buffer, &active, " "),
            Event::HardBreak => insert(buffer, &active, "\n"),
            Event::Rule => insert(buffer, &[], "\u{2014}\u{2014}\u{2014}\n\n"),
            Event::TaskListMarker(done) => {
                insert(buffer, &[], if done { "[x] " } else { "[ ] " });
            }
            // Raw HTML and anything else (footnotes, math) has no fallback
            // representation here; drop it rather than dump markup as text.
            _ => {}
        }
    }
}

/// Insert `text` at the end of `buffer` with every tag in `active` applied.
fn insert(buffer: &TextBuffer, active: &[TextTag], text: &str) {
    let refs: Vec<&TextTag> = active.iter().collect();
    let mut end = buffer.end_iter();
    buffer.insert_with_tags(&mut end, text, &refs);
}
