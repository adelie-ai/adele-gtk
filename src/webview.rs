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
        if decision_type == PolicyDecisionType::NavigationAction {
            if let Some(nav_decision) = decision.downcast_ref::<NavigationPolicyDecision>() {
                if let Some(mut action) = nav_decision.navigation_action() {
                    if let Some(request) = action.request() {
                        if let Some(uri) = request.uri() {
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
                    }
                }
            }
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

/// Scroll the webview to the bottom.
pub fn scroll_to_bottom(webview: &WebView) {
    webview.evaluate_javascript(
        "scrollToBottom();",
        None,
        None,
        None::<&gtk4::gio::Cancellable>,
        |_| {},
    );
}
