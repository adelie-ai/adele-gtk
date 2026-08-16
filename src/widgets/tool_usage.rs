//! Per-conversation tool-usage cost view (#144).
//!
//! Answers "what did the tools cost this conversation" on the two axes that do
//! not imply each other. Frequency is a signal on its own: forty calls to one
//! tool is a search loop or a retry storm even when each result is small.
//! Payload is the usual cause of a full context window: two calls that each
//! returned 40 KiB cost more than the forty small ones, and a count-ordered
//! list ranks them last. The view ranks by either axis on demand.
//!
//! The pure ranking, grouping and formatting logic lives in this module as
//! free functions with unit tests. The GTK widget is a thin renderer over
//! them, in the shape `widgets/knowledge_browser.rs` established: its own
//! state cell, an mpsc pump fed by `bridge.spawn` transport calls, and no
//! dependence on the shared reducer.

use desktop_assistant_api_model as api;

/// Shown in place of the list when the conversation made no tool calls. An
/// empty conversation is a normal outcome, not a failure, so it never reaches
/// the error path.
pub const EMPTY_STATE: &str = "No tool calls in this conversation.";

/// What the view says about a group whose hosting server the daemon did not
/// report. Deliberately not a server name: no server is called this, and a
/// reader must not go looking for one.
pub const UNRESOLVED_NAMESPACE_TITLE: &str = "Server not recorded";

/// The line under an unresolved group, so the heading reads as missing data
/// rather than as a server that behaved oddly.
pub const UNRESOLVED_NAMESPACE_HINT: &str =
    "The daemon did not report which server hosts these tools.";

/// Which axis the view ranks by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortAxis {
    /// Estimated result tokens still resident. The "what ate my context"
    /// reading, and the default.
    Tokens,
    /// Calls the model requested, failures included.
    Calls,
}

impl SortAxis {
    /// This row's figure on the axis.
    pub fn value_of(self, _row: &api::ToolUsageView) -> u64 {
        0
    }

    /// The axis name for the sort control.
    pub fn label(self) -> &'static str {
        ""
    }
}

/// The server a group of tools belongs to, with the unresolved case kept
/// distinct from a named one instead of being folded into a placeholder name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NamespaceKey {
    /// The daemon named the hosting server.
    Named(String),
    /// The daemon reported no server for these tools.
    Unresolved,
}

impl NamespaceKey {
    /// The heading text for this key.
    pub fn title(&self) -> &str {
        ""
    }
}

/// One namespace's tools, ranked, with the subtotals that answer "which server
/// is this conversation leaning on".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceGroup {
    pub key: NamespaceKey,
    /// The group's tools, ranked by the requested axis, heaviest first.
    pub rows: Vec<api::ToolUsageView>,
    pub subtotal_calls: u64,
    pub subtotal_tokens: u64,
    pub subtotal_bytes: u64,
}

/// The header figures: how many distinct tools the conversation used, how many
/// calls it made, and what those calls left resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub distinct_tools: usize,
    pub total_calls: u64,
    pub total_tokens: u64,
}

/// Rank rows on `axis`, heaviest first.
///
/// Ties break on the other axis, then on the tool name, so the order is
/// stable for a caller that re-sorts the same data.
pub fn sort_rows(_rows: &mut [api::ToolUsageView], _axis: SortAxis) {}

/// The server key for a row. A namespace that is absent, empty, or only
/// whitespace is unresolved, not a server with a blank name.
pub fn namespace_key(_row: &api::ToolUsageView) -> NamespaceKey {
    NamespaceKey::Unresolved
}

/// Group rows by server and rank both the groups and the rows inside them.
///
/// Groups are ordered by their heaviest row on the axis, so the tool that
/// tops the whole view is always the first row on screen. The subtotal
/// breaks a tie, then the key.
pub fn group_by_namespace(_rows: &[api::ToolUsageView], _axis: SortAxis) -> Vec<NamespaceGroup> {
    Vec::new()
}

/// The largest single figure on the axis, which is what the bars scale
/// against so a bar means the same thing in every group.
pub fn peak_axis_value(_rows: &[api::ToolUsageView], _axis: SortAxis) -> u64 {
    0
}

/// The header figures for the whole conversation.
pub fn totals(_rows: &[api::ToolUsageView]) -> Totals {
    Totals::default()
}

/// The header line: distinct tools, calls, and resident tokens.
pub fn format_totals(_totals: &Totals) -> String {
    String::new()
}

/// The group heading: the server, its tool count, and its subtotals.
pub fn format_group_heading(_group: &NamespaceGroup) -> String {
    String::new()
}

/// How much of the bar this figure fills, against the view's peak.
///
/// Zero when nothing was used, so an all-zero conversation draws empty bars
/// rather than dividing by zero or drawing every bar full.
pub fn bar_fraction(_value: u64, _peak: u64) -> f64 {
    0.0
}

/// The under-reporting note for a row with evicted results, or `None`.
///
/// Compaction (#240) replaced these results with a pointer. The client cannot
/// tell an eviction that kept its bytes from one that did not, and the
/// original size of the second kind is not recoverable, so the note says the
/// resident figures MAY under-report rather than claiming a number it does
/// not have. Peak cost is desktop-assistant#675; nothing here estimates it.
pub fn eviction_note(_row: &api::ToolUsageView) -> Option<String> {
    None
}

/// Bytes as a person reads them: `845 B`, `1.2 KiB`, `3.4 MiB`.
pub fn format_bytes(_bytes: u64) -> String {
    String::new()
}

/// A count with thousands separators, so a five-figure token count is legible
/// at a glance.
pub fn format_thousands(_value: u64) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        tool_name: &str,
        namespace: Option<&str>,
        call_count: u32,
        result_bytes: u64,
        result_tokens: u64,
    ) -> api::ToolUsageView {
        api::ToolUsageView {
            tool_name: tool_name.to_string(),
            namespace: namespace.map(str::to_string),
            tool_tier: None,
            call_count,
            result_bytes,
            result_tokens,
            max_result_bytes: result_bytes,
            evicted_results: 0,
            first_ordinal: 0,
            last_ordinal: 0,
            first_used_at: None,
            last_used_at: None,
        }
    }

    /// A chatty tool with small results and a rare tool with huge ones: the
    /// pair the two axes exist to separate.
    fn mixed_call_mix() -> Vec<api::ToolUsageView> {
        vec![
            row("list_dir", Some("fileio"), 40, 20_480, 5_120),
            row("fetch_page", Some("web"), 2, 81_920, 20_480),
            row("read_file", Some("fileio"), 5, 4_096, 1_024),
        ]
    }

    fn flat_names(groups: &[NamespaceGroup]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| r.tool_name.clone())
            .collect()
    }

    // --- Acceptance criteria ---------------------------------------------

    #[test]
    fn a_known_call_mix_renders_one_row_per_tool_with_correct_counts_and_tokens() {
        let rows = mixed_call_mix();
        let groups = group_by_namespace(&rows, SortAxis::Tokens);

        let mut rendered: Vec<(String, u32, u64)> = groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| (r.tool_name.clone(), r.call_count, r.result_tokens))
            .collect();
        rendered.sort();

        assert_eq!(
            rendered,
            vec![
                ("fetch_page".to_string(), 2, 20_480),
                ("list_dir".to_string(), 40, 5_120),
                ("read_file".to_string(), 5, 1_024),
            ],
            "every tool appears exactly once, with the counts and tokens it was given"
        );

        let totals = totals(&rows);
        assert_eq!(totals.distinct_tools, 3);
        assert_eq!(totals.total_calls, 47);
        assert_eq!(totals.total_tokens, 26_624);
    }

    #[test]
    fn sorting_by_token_cost_puts_an_infrequent_but_huge_tool_first() {
        let rows = mixed_call_mix();

        let mut ranked = rows.clone();
        sort_rows(&mut ranked, SortAxis::Tokens);
        assert_eq!(ranked[0].tool_name, "fetch_page");

        // The grouped view is what a person actually reads, and the heaviest
        // tool must top it even though its server has the smaller subtotal.
        let groups = group_by_namespace(&rows, SortAxis::Tokens);
        assert_eq!(
            flat_names(&groups),
            vec!["fetch_page", "list_dir", "read_file"]
        );
    }

    #[test]
    fn sorting_by_call_count_puts_the_chatty_tool_first() {
        let rows = mixed_call_mix();

        let mut ranked = rows.clone();
        sort_rows(&mut ranked, SortAxis::Calls);
        assert_eq!(ranked[0].tool_name, "list_dir");

        let groups = group_by_namespace(&rows, SortAxis::Calls);
        assert_eq!(
            flat_names(&groups),
            vec!["list_dir", "read_file", "fetch_page"]
        );
    }

    #[test]
    fn namespace_grouping_shows_correct_subtotals() {
        let groups = group_by_namespace(&mixed_call_mix(), SortAxis::Tokens);
        assert_eq!(groups.len(), 2);

        let fileio = groups
            .iter()
            .find(|g| g.key == NamespaceKey::Named("fileio".into()))
            .expect("fileio group");
        assert_eq!(fileio.rows.len(), 2);
        assert_eq!(fileio.subtotal_calls, 45);
        assert_eq!(fileio.subtotal_tokens, 6_144);
        assert_eq!(fileio.subtotal_bytes, 24_576);

        let web = groups
            .iter()
            .find(|g| g.key == NamespaceKey::Named("web".into()))
            .expect("web group");
        assert_eq!(web.rows.len(), 1);
        assert_eq!(web.subtotal_calls, 2);
        assert_eq!(web.subtotal_tokens, 20_480);
        assert_eq!(web.subtotal_bytes, 81_920);
    }

    #[test]
    fn a_tool_with_evicted_results_is_marked_as_under_reported() {
        let mut evicted = row("fetch_page", Some("web"), 4, 8_192, 2_048);
        evicted.evicted_results = 3;
        let note = eviction_note(&evicted).expect("a row with evictions carries a note");
        assert!(note.contains('3'), "the note names how many: {note}");
        assert!(
            note.contains("evicted"),
            "the note names what happened: {note}"
        );
        assert!(
            note.contains("under-report"),
            "the note says the figures are a floor, not the whole story: {note}"
        );

        let intact = row("fetch_page", Some("web"), 4, 8_192, 2_048);
        assert_eq!(eviction_note(&intact), None);
    }

    #[test]
    fn an_empty_conversation_shows_the_empty_state_not_an_error() {
        let none: Vec<api::ToolUsageView> = Vec::new();
        assert!(group_by_namespace(&none, SortAxis::Tokens).is_empty());
        assert_eq!(totals(&none), Totals::default());
        assert_eq!(peak_axis_value(&none, SortAxis::Tokens), 0);
        assert_eq!(EMPTY_STATE, "No tool calls in this conversation.");
        assert!(
            !EMPTY_STATE.to_lowercase().contains("error"),
            "an empty conversation is a normal outcome"
        );
    }

    // --- Supporting behaviour --------------------------------------------

    #[test]
    fn an_unreported_namespace_is_not_presented_as_a_server_named_unknown() {
        for missing in [None, Some(""), Some("   ")] {
            let r = row("some_tool", missing, 1, 10, 3);
            assert_eq!(namespace_key(&r), NamespaceKey::Unresolved);
        }
        let title = NamespaceKey::Unresolved.title();
        assert_eq!(title, UNRESOLVED_NAMESPACE_TITLE);
        assert!(
            !title.to_lowercase().contains("unknown"),
            "no server is called this: {title}"
        );
        assert_eq!(NamespaceKey::Named("web".into()).title(), "web");
    }

    #[test]
    fn every_row_groups_under_the_unresolved_key_when_the_daemon_reports_no_server() {
        let rows = vec![
            row("list_dir", None, 40, 20_480, 5_120),
            row("fetch_page", None, 2, 81_920, 20_480),
        ];
        let groups = group_by_namespace(&rows, SortAxis::Tokens);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, NamespaceKey::Unresolved);
        assert_eq!(flat_names(&groups), vec!["fetch_page", "list_dir"]);
    }

    #[test]
    fn axis_value_reads_the_axis_it_names() {
        let r = row("t", None, 7, 100, 25);
        assert_eq!(SortAxis::Tokens.value_of(&r), 25);
        assert_eq!(SortAxis::Calls.value_of(&r), 7);
    }

    #[test]
    fn ranking_breaks_ties_deterministically() {
        let mut rows = vec![
            row("zulu", None, 3, 100, 50),
            row("alpha", None, 3, 100, 50),
            row("mike", None, 9, 100, 50),
        ];
        sort_rows(&mut rows, SortAxis::Tokens);
        let names: Vec<&str> = rows.iter().map(|r| r.tool_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["mike", "alpha", "zulu"],
            "equal tokens break on calls, then on name"
        );
    }

    #[test]
    fn peak_axis_value_scales_the_bars_against_the_heaviest_row() {
        let rows = mixed_call_mix();
        assert_eq!(peak_axis_value(&rows, SortAxis::Tokens), 20_480);
        assert_eq!(peak_axis_value(&rows, SortAxis::Calls), 40);
    }

    #[test]
    fn bar_fraction_is_proportional_and_safe_at_zero() {
        assert!((bar_fraction(0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((bar_fraction(5, 0) - 0.0).abs() < f64::EPSILON);
        assert!((bar_fraction(50, 100) - 0.5).abs() < 1e-9);
        assert!((bar_fraction(100, 100) - 1.0).abs() < 1e-9);
        assert!(
            (bar_fraction(200, 100) - 1.0).abs() < 1e-9,
            "a fraction never leaves the trough"
        );
    }

    #[test]
    fn format_bytes_scales_the_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(845), "845 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(3_670_016), "3.5 MiB");
        assert_eq!(format_bytes(2_147_483_648), "2.0 GiB");
    }

    #[test]
    fn format_thousands_groups_digits() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(26_624), "26,624");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn header_totals_name_both_axes() {
        let line = format_totals(&Totals {
            distinct_tools: 3,
            total_calls: 47,
            total_tokens: 26_624,
        });
        assert!(line.contains("3 tools"), "{line}");
        assert!(line.contains("47 calls"), "{line}");
        assert!(line.contains("26,624 tokens"), "{line}");
    }

    #[test]
    fn header_totals_read_in_the_singular_for_one() {
        let line = format_totals(&Totals {
            distinct_tools: 1,
            total_calls: 1,
            total_tokens: 1,
        });
        assert!(line.contains("1 tool "), "{line}");
        assert!(line.contains("1 call "), "{line}");
        assert!(line.contains("1 token"), "{line}");
    }

    #[test]
    fn group_heading_carries_the_server_and_its_subtotals() {
        let groups = group_by_namespace(&mixed_call_mix(), SortAxis::Tokens);
        let fileio = groups
            .iter()
            .find(|g| g.key == NamespaceKey::Named("fileio".into()))
            .expect("fileio group");
        let heading = format_group_heading(fileio);
        assert!(heading.contains("fileio"), "{heading}");
        assert!(heading.contains("2 tools"), "{heading}");
        assert!(heading.contains("45 calls"), "{heading}");
        assert!(heading.contains("6,144 tokens"), "{heading}");
    }

    #[test]
    fn the_unresolved_group_explains_itself_without_naming_a_server() {
        assert!(
            UNRESOLVED_NAMESPACE_HINT.contains("did not report"),
            "the hint says the data is missing: {UNRESOLVED_NAMESPACE_HINT}"
        );
        assert!(
            !UNRESOLVED_NAMESPACE_HINT.to_lowercase().contains("unknown"),
            "the hint must not read as a server name: {UNRESOLVED_NAMESPACE_HINT}"
        );
    }

    #[test]
    fn sort_axis_labels_name_the_two_readings() {
        assert_eq!(SortAxis::Tokens.label(), "Token cost");
        assert_eq!(SortAxis::Calls.label(), "Call count");
    }
}
