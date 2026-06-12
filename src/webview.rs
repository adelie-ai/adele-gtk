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

/// Update the webview with rendered messages HTML.
pub fn update_messages(webview: &WebView, messages_html: &str) {
    let js = format!("updateMessages({});", js_safe_string(messages_html));
    webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
}

/// Append a streaming chunk to the webview.
pub fn append_chunk(webview: &WebView, chunk: &str) {
    let js = format!("appendChunk({});", js_safe_string(chunk));
    webview.evaluate_javascript(&js, None, None, None::<&gtk4::gio::Cancellable>, |_| {});
}

/// Show a transient status message below the chat (e.g. "Searching knowledge base...").
pub fn set_status(webview: &WebView, message: &str) {
    let js = format!("setStatus({});", js_safe_string(message));
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

    /// `js_safe_string` is the security boundary that prevents a hostile
    /// assistant/tool message from breaking out of the JS string literal it is
    /// interpolated into. Its contract is the JSON-string contract: the output
    /// is a quoted JSON string literal that parses back to *exactly* the input.
    /// This property-style test drives it with a representative input set that
    /// covers every C0 control char (0x00–0x1F), quotes, backslashes, the
    /// line/paragraph separators that are valid JSON but break naive JS string
    /// literals, and non-ASCII, asserting the round-trip holds for each.
    #[test]
    fn js_safe_string_round_trips_through_json_for_all_control_chars() {
        // Build the representative corpus.
        let mut inputs: Vec<String> = Vec::new();

        // Every C0 control character, individually and embedded in text.
        for code in 0x00u32..=0x1F {
            let c = char::from_u32(code).unwrap();
            inputs.push(c.to_string());
            inputs.push(format!("before{c}after"));
        }

        // Quotes, backslashes, slashes, and combinations that classically
        // escape JS string / template literals.
        for s in [
            "\"",
            "'",
            "\\",
            "\\\"",
            "\\n",           // literal backslash-n, not a newline
            "</script>",     // HTML closer
            "`${alert(1)}`", // JS template-literal injection
            "\u{2028}",      // LINE SEPARATOR (breaks naive JS string literals)
            "\u{2029}",      // PARAGRAPH SEPARATOR
            "\u{FEFF}",      // BOM / zero-width no-break space
            "\0embedded\0null",
            "emoji 🦀 and accents éàü",
            "中文字符",
            "",
        ] {
            inputs.push(s.to_string());
        }

        for input in &inputs {
            let encoded = js_safe_string(input);
            // The output must be a self-contained JSON string literal that
            // parses back to the exact original — the guarantee callers rely on
            // when they do `format!("appendChunk({});", js_safe_string(x))`.
            let decoded: String = serde_json::from_str(&encoded).unwrap_or_else(|e| {
                panic!("js_safe_string({input:?}) -> {encoded:?} did not parse as JSON: {e}")
            });
            assert_eq!(
                &decoded, input,
                "round-trip mismatch: input {input:?} encoded as {encoded:?} decoded as {decoded:?}"
            );
            // Defensive: a raw control char must never survive into the output
            // unescaped, or it could terminate/derange the JS literal.
            for code in 0x00u32..=0x1F {
                let c = char::from_u32(code).unwrap();
                if input.contains(c) {
                    assert!(
                        !encoded.contains(c),
                        "control char {code:#04x} leaked unescaped into {encoded:?}"
                    );
                }
            }
        }
    }
}
