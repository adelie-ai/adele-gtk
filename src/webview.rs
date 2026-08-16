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

// The statements below are evaluated by the WebView, so an assistant or tool
// message reaching one is untrusted text inside a program. Encoding it is
// `adele_markdown::js::string_literal`'s job, not this module's: host-side
// evaluation is exempt from the page's pinned `script-src`, so this is the last
// layer, and a second copy of the escaper here is exactly the drift the shared
// crate exists to prevent (gtk#25).

/// Build the `updateMessages(...)` statement for a rendered transcript.
fn update_messages_script(messages_html: &str) -> String {
    format!(
        "updateMessages({});",
        adele_markdown::js::string_literal(messages_html)
    )
}

/// Build the `appendChunk(...)` statement for a streaming chunk.
fn append_chunk_script(chunk: &str) -> String {
    format!(
        "appendChunk({});",
        adele_markdown::js::string_literal(chunk)
    )
}

/// Build the `setStatus(...)` statement for a transient status line.
fn set_status_script(message: &str) -> String {
    format!(
        "setStatus({});",
        adele_markdown::js::string_literal(message)
    )
}

/// Build the transcript hit-test statement for a pointer position, in CSS
/// pixels relative to the WebView.
///
/// It reports the `data-turn-index` of the message under the pointer (`-1`
/// when the pointer is over the page background) and the page's current
/// selection, separated by the first newline. `x` and `y` are narrowed to
/// integers before they are written into the statement, so the only thing this
/// interpolates is two numbers.
fn transcript_click_script(x: f64, y: f64) -> String {
    let x = x as i32;
    let y = y as i32;
    format!(
        "(function(){{\
         var e=document.elementFromPoint({x},{y});\
         var m=(e&&e.closest)?e.closest('[data-turn-index]'):null;\
         var i=m?m.getAttribute('data-turn-index'):'-1';\
         var s=window.getSelection?String(window.getSelection()):'';\
         return i+'\\n'+s;}})()"
    )
}

/// Resolve which transcript message is under a pointer position, and what the
/// page has selected, then hand both to `on_result` on the GTK main loop.
///
/// Asynchronous because the DOM lives in the web process. A statement that
/// fails (the page is still loading, the web process died) reports no entry
/// and no selection rather than guessing at one.
pub fn query_transcript_click(
    webview: &WebView,
    x: f64,
    y: f64,
    on_result: impl FnOnce(crate::transcript::TranscriptClick) + 'static,
) {
    let js = transcript_click_script(x, y);
    webview.evaluate_javascript(
        &js,
        None,
        None,
        None::<&gtk4::gio::Cancellable>,
        move |result| {
            let click = match result {
                Ok(value) => crate::transcript::parse_transcript_click(&value.to_str()),
                Err(e) => {
                    tracing::warn!("transcript hit test failed: {e}");
                    crate::transcript::TranscriptClick::default()
                }
            };
            on_result(click);
        },
    );
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
    use crate::transcript::TranscriptEntry;
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
        let messages = vec![TranscriptEntry::new(
            "assistant",
            hostile,
            MessageKind::Normal,
            None,
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

    // --- gtk#169: the transcript hit test ---------------------------------

    #[test]
    fn the_hit_test_statement_carries_the_pointer_position() {
        let js = transcript_click_script(12.7, 34.2);
        assert!(
            js.contains("elementFromPoint(12,34)"),
            "the pointer position must reach the statement: {js:?}"
        );
        assert!(js.contains("data-turn-index"), "{js:?}");
    }

    #[test]
    fn the_hit_test_statement_interpolates_nothing_but_numbers() {
        // The statement is evaluated host-side, which the page's pinned
        // `script-src` does not cover, so anything interpolated into it runs
        // as code. Only the two coordinates are, and they are integers by the
        // time they get there.
        for (x, y) in [(-1.0_f64, -1.0_f64), (0.0, 0.0), (4096.9, 2160.9)] {
            let js = transcript_click_script(x, y);
            let point = js
                .split_once("elementFromPoint(")
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(args, _)| args.to_string())
                .unwrap_or_else(|| panic!("no hit-test call in {js:?}"));
            for arg in point.split(',') {
                arg.trim()
                    .parse::<i32>()
                    .unwrap_or_else(|e| panic!("{arg:?} is not an integer: {e}"));
            }
        }
    }
}
