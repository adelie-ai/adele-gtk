use pulldown_cmark::{Options, Parser, html};

/// Convert markdown text to HTML.
pub fn markdown_to_html(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);
    html_output
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
        format!(r#"<div class="avatar avatar-fallback">{}</div>"#, &alt[..1])
    } else {
        let safe_url = html_escape_attr(url);
        let safe_alt = html_escape_attr(alt);
        format!(r#"<img class="avatar" src="{safe_url}" alt="{safe_alt}">"#)
    }
}

/// Render a full set of chat messages into an HTML document body.
pub fn render_messages_html(
    messages: &[(String, String)],
    streaming_buffer: Option<&str>,
    avatars: &AvatarUrls,
) -> String {
    let mut html = String::new();

    for (role, content) in messages {
        let (class, label, avatar_html) = match role.as_str() {
            "user" => (
                "message user-message",
                "You",
                avatar_img(&avatars.user, "You"),
            ),
            "assistant" => (
                "message assistant-message",
                "Adele",
                avatar_img(&avatars.adele, "Adele"),
            ),
            _ => ("message", "", String::new()),
        };

        let content_html = markdown_to_html(content);
        html.push_str(&format!(
            r#"<div class="{class}">{avatar_html}<div class="bubble"><div class="label">{label}</div><div class="content">{content_html}</div></div></div>"#
        ));
    }

    if let Some(buffer) = streaming_buffer {
        if !buffer.is_empty() {
            let content_html = markdown_to_html(buffer);
            let avatar_html = avatar_img(&avatars.adele, "Adele");
            html.push_str(&format!(
                r#"<div class="message assistant-message streaming">{avatar_html}<div class="bubble"><div class="label">Adele</div><div class="content">{content_html}<span class="cursor">▌</span></div></div></div>"#
            ));
        }
    }

    html
}

/// Full HTML page template with embedded CSS.
pub fn html_template() -> &'static str {
    r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: file:; script-src 'unsafe-inline';">
<style>
* { margin: 0; padding: 0; box-sizing: border-box; }

body {
    background: #1a1d2e;
    color: #e0e0e0;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 14px;
    line-height: 1.6;
    padding: 16px;
}

#messages {
    display: flex;
    flex-direction: column;
    gap: 16px;
}

.message {
    display: flex;
    align-items: flex-start;
    gap: 10px;
}

.avatar {
    width: 28px;
    height: 28px;
    min-width: 28px;
    border-radius: 50%;
    object-fit: cover;
    object-position: center 15%;
    margin-top: 2px;
}

.avatar-fallback {
    background: #3a3f5c;
    color: #9ca3af;
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 600;
    font-size: 13px;
}

.bubble {
    flex: 1;
    min-width: 0;
    border-radius: 8px;
    padding: 12px 16px;
}

.user-message .bubble {
    background: rgba(255, 189, 89, 0.08);
    border-left: 3px solid #ffbd59;
}

.user-message .label {
    color: #ffbd59;
    font-weight: 600;
    margin-bottom: 4px;
}

.assistant-message .bubble {
    background: rgba(92, 206, 154, 0.08);
    border-left: 3px solid #5cce9a;
}

.assistant-message .label {
    color: #5cce9a;
    font-weight: 600;
    margin-bottom: 4px;
}

.assistant-message.streaming .bubble {
    border-left-color: #84dac1;
}

.assistant-message.streaming .label {
    color: #84dac1;
}

.content p { margin: 0.5em 0; }
.content p:first-child { margin-top: 0; }
.content p:last-child { margin-bottom: 0; }

.content pre {
    background: #232740;
    border-radius: 6px;
    padding: 12px;
    overflow-x: auto;
    margin: 0.5em 0;
}

.content code {
    font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
    font-size: 13px;
}

.content :not(pre) > code {
    background: #232740;
    padding: 2px 6px;
    border-radius: 3px;
}

.content ul, .content ol {
    padding-left: 1.5em;
    margin: 0.5em 0;
}

.content table {
    border-collapse: collapse;
    margin: 0.5em 0;
}

.content th, .content td {
    border: 1px solid #3a3f5c;
    padding: 6px 12px;
}

.content th {
    background: #232740;
}

.content a {
    color: #7aa3ff;
    text-decoration: none;
}

.content a:hover {
    text-decoration: underline;
}

.cursor {
    color: #84dac1;
    animation: blink 1s step-end infinite;
}

@keyframes blink {
    50% { opacity: 0; }
}

#status-indicator {
    display: none;
    padding: 8px 16px;
    color: #9ca3af;
    font-size: 13px;
    font-style: italic;
}

#status-indicator.visible {
    display: flex;
    align-items: center;
    gap: 8px;
}

#status-indicator .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #84dac1;
    animation: pulse 1.5s ease-in-out infinite;
}

@keyframes pulse {
    0%, 100% { opacity: 0.4; }
    50% { opacity: 1; }
}
</style>
</head>
<body>
<div id="messages"></div>
<div id="status-indicator"><span class="dot"></span><span id="status-text"></span></div>
<script>
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
</script>
</body>
</html>"##
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
            ("user".to_string(), "Hello".to_string()),
            ("assistant".to_string(), "Hi there!".to_string()),
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
            ("user".to_string(), "Hi".to_string()),
            ("assistant".to_string(), "Hello".to_string()),
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
        let messages = vec![("user".to_string(), "Hi".to_string())];
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
}
