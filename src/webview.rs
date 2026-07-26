use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::{NavigationPolicyDecision, PolicyDecisionType, WebView};

use crate::markdown;

/// Create and configure a WebView for rendering chat messages.
pub fn create_chat_webview() -> WebView {
    let webview = WebView::new();

    // Load the HTML template
    webview.load_html(markdown::html_template(), None);

    // Intercept navigation to open external links in the default browser
    webview.connect_decide_policy(|_webview, decision, decision_type| {
        if decision_type == PolicyDecisionType::NavigationAction
            && let Some(nav_decision) = decision.downcast_ref::<NavigationPolicyDecision>()
            && let Some(action) = nav_decision.navigation_action()
            && let Some(request) = action.request()
            && let Some(uri) = request.uri()
        {
            let uri_str = uri.as_str();
            // Allow internal navigation (initial page load)
            if uri_str == "about:blank"
                || uri_str.starts_with("data:")
                || uri_str.starts_with("file:")
            {
                return false; // allow
            }

            // Open external links in default browser
            let _ = gtk4::gio::AppInfo::launch_default_for_uri(
                uri_str,
                gtk4::gio::AppLaunchContext::NONE,
            );
            decision.ignore();
            return true; // handled
        }
        false
    });

    webview
}

/// JSON-encode a string so it is safe to interpolate into JavaScript.
/// `serde_json::to_string` produces a quoted, properly escaped JSON string
/// literal which is also a valid JavaScript string literal — no manual
/// escaping of backticks, backslashes, or template expressions needed.
fn js_safe_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Build the `updateMessages(...)` statement for a rendered transcript.
fn update_messages_script(messages_html: &str) -> String {
    format!("updateMessages({});", js_safe_string(messages_html))
}

/// Build the `appendChunk(...)` statement for a streaming chunk.
fn append_chunk_script(chunk: &str) -> String {
    format!("appendChunk({});", js_safe_string(chunk))
}

/// Build the `setStatus(...)` statement for a transient status line.
fn set_status_script(message: &str) -> String {
    format!("setStatus({});", js_safe_string(message))
}

/// Update the webview with rendered messages HTML.
pub fn update_messages(webview: &WebView, messages_html: &str) {
    let js = update_messages_script(messages_html);
    webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
}

/// Append a streaming chunk to the webview.
pub fn append_chunk(webview: &WebView, chunk: &str) {
    let js = append_chunk_script(chunk);
    webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
}

/// Show a transient status message below the chat (e.g. "Searching knowledge base...").
pub fn set_status(webview: &WebView, message: &str) {
    let js = set_status_script(message);
    webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
}

/// Clear the transient status indicator.
pub fn clear_status(webview: &WebView) {
    webview.evaluate_javascript(
        "clearStatus();",
        None,
        None,
        None::<&gtk4::gio::Cancellable>,
        |_| {},
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{AvatarUrls, render_messages_html};
    use desktop_assistant_client_common::MessageKind;

    /// Every statement this module evaluates, for one message body.
    fn statements(body: &str) -> Vec<String> {
        vec![
            update_messages_script(body),
            append_chunk_script(body),
            set_status_script(body),
        ]
    }

    #[test]
    fn a_js_line_separator_in_a_message_cannot_end_the_statement() {
        // U+2028 / U+2029 are legal *unescaped* inside a JSON string but are
        // JavaScript line terminators, so a JSON encoder alone leaves a reply
        // able to end the statement it was interpolated into. The shared
        // encoder escapes them; a client-local `serde_json::to_string` does not.
        for statement in statements("before\u{2028}after\u{2029}end") {
            assert!(
                !statement.contains('\u{2028}') && !statement.contains('\u{2029}'),
                "raw JS line separator survived into: {statement:?}"
            );
        }
    }

    #[test]
    fn a_hostile_message_stays_inside_the_js_string_literal() {
        // The end-to-end shape: a reply flows through the sanitizer into the
        // transcript markup, and that markup is interpolated into the call this
        // module evaluates. It must arrive as one argument, not as code.
        let hostile = "he said \"x\");alert(document.cookie);//\n\nand <b>more</b>";
        let messages = vec![(
            "assistant".to_string(),
            hostile.to_string(),
            MessageKind::Normal,
        )];
        let body = render_messages_html(
            &messages,
            None,
            &AvatarUrls {
                adele: String::new(),
                user: String::new(),
            },
        );

        for statement in statements(&body) {
            let (open, inner) = statement
                .split_once('(')
                .expect("statement is a function call");
            let inner = inner
                .strip_suffix(");")
                .unwrap_or_else(|| panic!("statement is one call: {statement:?}"));
            let decoded: String = serde_json::from_str(inner).unwrap_or_else(|e| {
                panic!("{open} argument is not a single string literal: {e}: {statement:?}")
            });
            assert_eq!(decoded, body, "the message must arrive intact as data");
            assert!(!statement.contains('\n'), "no raw newline: {statement:?}");
        }
    }

    #[test]
    fn every_statement_is_built_through_the_shared_encoder() {
        // Drift is the failure mode this repo's markdown lift exists to stop:
        // a client-local escaper diverges from the shared one silently. Pin
        // that these builders delegate rather than quote for themselves.
        let body = "quotes \" and \\ and </script> and \u{2028}";
        let encoded = adele_markdown::js::string_literal(body);
        assert_eq!(
            update_messages_script(body),
            format!("updateMessages({encoded});")
        );
        assert_eq!(
            append_chunk_script(body),
            format!("appendChunk({encoded});")
        );
        assert_eq!(set_status_script(body), format!("setStatus({encoded});"));
    }
}
