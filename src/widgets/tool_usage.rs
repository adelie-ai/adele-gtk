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

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::Connector;
use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, Button, DropDown, Expander, Grid, HeaderBar, Label,
    Orientation, ProgressBar, ScrolledWindow, StringList, Window, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::AsyncBridge;
use crate::management_client;

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

/// The sort options, in the order the control offers them. The first is the
/// default: "what ate my context" is the question this view exists for.
const SORT_AXES: [SortAxis; 2] = [SortAxis::Tokens, SortAxis::Calls];

/// Width of the bar column. Wide enough that a short bar is still visible,
/// narrow enough to leave the figures room on a 900 px window.
const BAR_WIDTH_PX: i32 = 160;

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
    pub fn value_of(self, row: &api::ToolUsageView) -> u64 {
        match self {
            Self::Tokens => row.result_tokens,
            Self::Calls => u64::from(row.call_count),
        }
    }

    /// The other axis, which breaks a tie in the ranking.
    fn other(self) -> Self {
        match self {
            Self::Tokens => Self::Calls,
            Self::Calls => Self::Tokens,
        }
    }

    /// The axis name for the sort control.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Token cost",
            Self::Calls => "Call count",
        }
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
        match self {
            Self::Named(name) => name,
            Self::Unresolved => UNRESOLVED_NAMESPACE_TITLE,
        }
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

/// Internal messages from async work back to the main thread.
enum UsageMsg {
    Loaded(Vec<api::ToolUsageView>),
    Error(String),
}

/// What the window is currently showing.
struct UsageState {
    rows: Vec<api::ToolUsageView>,
    axis: SortAxis,
}

/// The tool-usage cost window for one conversation.
///
/// Non-modal, like the knowledge browser, so a person can keep the reading in
/// view while they carry on with the conversation it describes.
pub struct ToolUsageWindow {
    pub window: Window,
}

impl ToolUsageWindow {
    /// Build the window for `conversation_id`. The caller presents it.
    pub fn new(
        parent: &ApplicationWindow,
        conversation_id: String,
        transport: Arc<Connector>,
        bridge: Rc<AsyncBridge>,
    ) -> Self {
        let window = Window::builder()
            .title("Tool Usage")
            .transient_for(parent)
            .modal(false)
            .default_width(900)
            .default_height(560)
            .build();

        let header = HeaderBar::new();
        let title_label = Label::new(Some("Tool Usage"));
        title_label.add_css_class("title");
        header.set_title_widget(Some(&title_label));
        window.set_titlebar(Some(&header));

        let refresh_button = Button::from_icon_name("view-refresh-symbolic");
        refresh_button.set_tooltip_text(Some("Reload"));
        header.pack_start(&refresh_button);

        let sort_box = GtkBox::new(Orientation::Horizontal, 6);
        let sort_caption = Label::new(Some("Sort by"));
        sort_caption.add_css_class("dim-label");
        sort_box.append(&sort_caption);
        let sort_model = StringList::new(&SORT_AXES.map(SortAxis::label));
        let sort_dropdown = DropDown::new(Some(sort_model), gtk4::Expression::NONE);
        sort_dropdown.set_selected(0);
        sort_dropdown.set_tooltip_text(Some(
            "Token cost ranks what filled the context window. Call count ranks \
             what the model kept going back to.",
        ));
        sort_box.append(&sort_dropdown);
        header.pack_end(&sort_box);

        let body = GtkBox::new(Orientation::Vertical, 0);

        let totals_label = Label::new(Some(""));
        totals_label.set_halign(Align::Start);
        totals_label.set_margin_start(12);
        totals_label.set_margin_end(12);
        totals_label.set_margin_top(10);
        totals_label.set_tooltip_text(Some(
            "Counted across the whole conversation. Bytes are measured; tokens are \
             estimated by the same rule the context budget uses, so the two figures \
             are comparable.",
        ));
        totals_label.add_css_class("tool-usage-totals");
        body.append(&totals_label);

        let status_label = Label::new(Some("Loading..."));
        status_label.set_halign(Align::Start);
        status_label.set_wrap(true);
        status_label.set_xalign(0.0);
        status_label.set_margin_start(12);
        status_label.set_margin_end(12);
        status_label.set_margin_top(4);
        status_label.set_margin_bottom(4);
        status_label.add_css_class("dim-label");
        body.append(&status_label);

        let scroll = ScrolledWindow::new();
        scroll.set_vexpand(true);
        let content = GtkBox::new(Orientation::Vertical, 10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(4);
        content.set_margin_bottom(12);
        scroll.set_child(Some(&content));
        body.append(&scroll);

        window.set_child(Some(&body));

        let state = Rc::new(RefCell::new(UsageState {
            rows: Vec::new(),
            axis: SORT_AXES[0],
        }));

        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<UsageMsg>();

        // Reload closure, shared by the initial load and the refresh button,
        // so both hold one copy of the captured transport / bridge / sender.
        let reload: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[weak]
            status_label,
            move || {
                status_label.set_text("Loading...");
                let transport = Arc::clone(&transport);
                let msg_tx = msg_tx.clone();
                let conversation_id = conversation_id.clone();
                bridge.spawn(async move {
                    let result =
                        management_client::get_tool_usage(transport.client(), conversation_id)
                            .await;
                    let _ = match result {
                        Ok(rows) => msg_tx.send(UsageMsg::Loaded(rows)),
                        Err(e) => msg_tx.send(UsageMsg::Error(e.to_string())),
                    };
                });
            }
        ));

        // Repaint from whatever is in state. Called after a load and after the
        // sort axis changes, so both routes render through one path.
        let repaint: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[strong]
            state,
            #[weak]
            content,
            #[weak]
            totals_label,
            #[weak]
            status_label,
            move || {
                let s = state.borrow();
                totals_label.set_text(&format_totals(&totals(&s.rows)));
                // An empty conversation is a normal outcome: the empty state
                // replaces the list, and nothing is drawn that could read as a
                // broken chart.
                status_label.set_text(if s.rows.is_empty() { EMPTY_STATE } else { "" });
                render_groups(&content, &s.rows, s.axis);
            }
        ));

        glib::spawn_future_local(glib::clone!(
            #[strong]
            state,
            #[strong]
            repaint,
            #[weak]
            status_label,
            async move {
                while let Some(msg) = msg_rx.recv().await {
                    match msg {
                        UsageMsg::Loaded(rows) => {
                            state.borrow_mut().rows = rows;
                            repaint();
                        }
                        UsageMsg::Error(e) => {
                            status_label.set_text(&format!("Error: {e}"));
                        }
                    }
                }
            }
        ));

        refresh_button.connect_clicked(glib::clone!(
            #[strong]
            reload,
            move |_| reload()
        ));

        sort_dropdown.connect_selected_notify(glib::clone!(
            #[strong]
            state,
            #[strong]
            repaint,
            move |dropdown| {
                let axis = SORT_AXES
                    .get(dropdown.selected() as usize)
                    .copied()
                    .unwrap_or(SORT_AXES[0]);
                state.borrow_mut().axis = axis;
                repaint();
            }
        ));

        reload();

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

// --- Rendering -------------------------------------------------------------

/// Rebuild the group list. One collapsible expander per server, each holding a
/// grid of that server's tools.
fn render_groups(container: &GtkBox, rows: &[api::ToolUsageView], axis: SortAxis) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    // Every bar in the window scales against one peak, so a bar means the same
    // length in every group and re-sorting visibly re-ranks the whole view.
    let peak = peak_axis_value(rows, axis);

    for group in group_by_namespace(rows, axis) {
        let expander = Expander::new(None);
        expander.set_expanded(true);
        let heading = Label::new(Some(&format_group_heading(&group)));
        heading.set_xalign(0.0);
        heading.add_css_class("tool-usage-group");
        expander.set_label_widget(Some(&heading));

        let inner = GtkBox::new(Orientation::Vertical, 4);
        inner.set_margin_start(16);
        inner.set_margin_top(4);

        if group.key == NamespaceKey::Unresolved {
            let hint = Label::new(Some(UNRESOLVED_NAMESPACE_HINT));
            hint.set_xalign(0.0);
            hint.set_wrap(true);
            hint.add_css_class("dim-label");
            inner.append(&hint);
        }

        let grid = Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(2);
        for (index, row) in group.rows.iter().enumerate() {
            attach_row(
                &grid,
                i32::try_from(index).unwrap_or(i32::MAX),
                row,
                axis,
                peak,
            );
        }
        inner.append(&grid);

        expander.set_child(Some(&inner));
        container.append(&expander);
    }
}

/// Attach one tool to the group grid: name, bar, and the figures behind it.
///
/// Each tool takes two grid rows so an under-reporting note can sit beneath
/// its own tool rather than in a separate list the reader has to match up.
fn attach_row(grid: &Grid, index: i32, row: &api::ToolUsageView, axis: SortAxis, peak: u64) {
    let line = index.saturating_mul(2);

    let name = Label::new(Some(&row.tool_name));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(28);
    name.set_width_chars(20);
    name.set_tooltip_text(Some(&row.tool_name));
    name.add_css_class("tool-usage-name");
    grid.attach(&name, 0, line, 1, 1);

    let bar = ProgressBar::new();
    bar.set_fraction(bar_fraction(axis.value_of(row), peak));
    bar.set_valign(Align::Center);
    bar.set_size_request(BAR_WIDTH_PX, -1);
    bar.set_tooltip_text(Some(&format!(
        "{}: {} of the heaviest tool in this conversation",
        axis.label(),
        format_percent(axis.value_of(row), peak)
    )));
    bar.add_css_class("tool-usage-bar");
    grid.attach(&bar, 1, line, 1, 1);

    attach_figure(
        grid,
        2,
        line,
        &pluralize(row.result_tokens, "token"),
        "Estimated tokens for the results still resident in this conversation.",
    );
    attach_figure(
        grid,
        3,
        line,
        &pluralize(u64::from(row.call_count), "call"),
        "Calls the model requested, failures included.",
    );
    attach_figure(
        grid,
        4,
        line,
        &format_bytes(row.result_bytes),
        "Result bytes still resident in this conversation.",
    );
    attach_figure(
        grid,
        5,
        line,
        &format!("max {}", format_bytes(row.max_result_bytes)),
        "The largest single resident result. A max close to the total means one \
         dump, not a steady trickle.",
    );

    if let Some(note) = eviction_note(row) {
        let warn = Label::new(Some(&note));
        warn.set_xalign(0.0);
        warn.set_wrap(true);
        warn.set_tooltip_text(Some(
            "Compaction replaced these results with a pointer. Where the \
             original bytes were overwritten their size is not recoverable, so \
             the figures above are a floor.",
        ));
        warn.add_css_class("tool-usage-warn");
        grid.attach(&warn, 0, line + 1, 6, 1);
    }
}

/// Attach one right-aligned figure cell with the tooltip that says what it is.
fn attach_figure(grid: &Grid, column: i32, line: i32, text: &str, tooltip: &str) {
    let label = Label::new(Some(text));
    label.set_xalign(1.0);
    label.set_tooltip_text(Some(tooltip));
    label.add_css_class("tool-usage-figure");
    grid.attach(&label, column, line, 1, 1);
}

// --- Pure helpers ----------------------------------------------------------

/// Rank rows on `axis`, heaviest first.
///
/// Ties break on the other axis, then on the tool name, so the order is
/// stable for a caller that re-sorts the same data.
pub fn sort_rows(rows: &mut [api::ToolUsageView], axis: SortAxis) {
    rows.sort_by(|a, b| {
        axis.value_of(b)
            .cmp(&axis.value_of(a))
            .then_with(|| axis.other().value_of(b).cmp(&axis.other().value_of(a)))
            .then_with(|| a.tool_name.cmp(&b.tool_name))
    });
}

/// The server key for a row. A namespace that is absent, empty, or only
/// whitespace is unresolved, not a server with a blank name.
pub fn namespace_key(row: &api::ToolUsageView) -> NamespaceKey {
    match row.namespace.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => NamespaceKey::Named(name.to_string()),
        _ => NamespaceKey::Unresolved,
    }
}

/// Group rows by server and rank both the groups and the rows inside them.
///
/// Groups are ordered by their heaviest row on the axis, so the tool that
/// tops the whole view is always the first row on screen. The subtotal
/// breaks a tie, then the key. Ordering groups by subtotal instead would
/// bury the single heaviest tool behind a server with more small ones, which
/// is the reading this view exists to give.
pub fn group_by_namespace(rows: &[api::ToolUsageView], axis: SortAxis) -> Vec<NamespaceGroup> {
    let mut buckets: BTreeMap<NamespaceKey, Vec<api::ToolUsageView>> = BTreeMap::new();
    for row in rows {
        buckets
            .entry(namespace_key(row))
            .or_default()
            .push(row.clone());
    }

    let mut groups: Vec<NamespaceGroup> = buckets
        .into_iter()
        .map(|(key, mut rows)| {
            sort_rows(&mut rows, axis);
            NamespaceGroup {
                subtotal_calls: rows.iter().map(|r| u64::from(r.call_count)).sum(),
                subtotal_tokens: rows.iter().map(|r| r.result_tokens).sum(),
                subtotal_bytes: rows.iter().map(|r| r.result_bytes).sum(),
                key,
                rows,
            }
        })
        .collect();

    groups.sort_by(|a, b| {
        let peak = |g: &NamespaceGroup| g.rows.iter().map(|r| axis.value_of(r)).max().unwrap_or(0);
        let subtotal = |g: &NamespaceGroup| match axis {
            SortAxis::Tokens => g.subtotal_tokens,
            SortAxis::Calls => g.subtotal_calls,
        };
        peak(b)
            .cmp(&peak(a))
            .then_with(|| subtotal(b).cmp(&subtotal(a)))
            .then_with(|| a.key.cmp(&b.key))
    });

    groups
}

/// The largest single figure on the axis, which is what the bars scale
/// against so a bar means the same thing in every group.
pub fn peak_axis_value(rows: &[api::ToolUsageView], axis: SortAxis) -> u64 {
    rows.iter().map(|r| axis.value_of(r)).max().unwrap_or(0)
}

/// The header figures for the whole conversation.
pub fn totals(rows: &[api::ToolUsageView]) -> Totals {
    let mut names: Vec<&str> = rows.iter().map(|r| r.tool_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    Totals {
        distinct_tools: names.len(),
        total_calls: rows.iter().map(|r| u64::from(r.call_count)).sum(),
        total_tokens: rows.iter().map(|r| r.result_tokens).sum(),
    }
}

/// The header line: distinct tools, calls, and resident tokens.
pub fn format_totals(totals: &Totals) -> String {
    format!(
        "{} - {} - {}",
        pluralize(totals.distinct_tools as u64, "tool"),
        pluralize(totals.total_calls, "call"),
        pluralize(totals.total_tokens, "token"),
    )
}

/// The group heading: the server, its tool count, and its subtotals.
pub fn format_group_heading(group: &NamespaceGroup) -> String {
    format!(
        "{} - {}, {}, {}, {}",
        group.key.title(),
        pluralize(group.rows.len() as u64, "tool"),
        pluralize(group.subtotal_calls, "call"),
        pluralize(group.subtotal_tokens, "token"),
        format_bytes(group.subtotal_bytes),
    )
}

/// How much of the bar this figure fills, against the view's peak.
///
/// Zero when nothing was used, so an all-zero conversation draws empty bars
/// rather than dividing by zero or drawing every bar full.
pub fn bar_fraction(value: u64, peak: u64) -> f64 {
    if peak == 0 {
        return 0.0;
    }
    (value as f64 / peak as f64).clamp(0.0, 1.0)
}

/// This figure as a percentage of the peak, for the bar's tooltip.
fn format_percent(value: u64, peak: u64) -> String {
    format!("{:.0}%", bar_fraction(value, peak) * 100.0)
}

/// The under-reporting note for a row with evicted results, or `None`.
///
/// Compaction (#240) replaced these results with a pointer. The client cannot
/// tell an eviction that kept its bytes from one that did not, and the
/// original size of the second kind is not recoverable, so the note says the
/// resident figures MAY under-report rather than claiming a number it does
/// not have. Peak cost is desktop-assistant#675; nothing here estimates it.
pub fn eviction_note(row: &api::ToolUsageView) -> Option<String> {
    if row.evicted_results == 0 {
        return None;
    }
    Some(format!(
        "{} evicted by compaction - the figures above may under-report what this tool cost",
        format_thousands(u64::from(row.evicted_results)),
    ))
}

/// Bytes as a person reads them: `845 B`, `1.2 KiB`, `3.4 MiB`.
///
/// The unit is chosen against the figure AFTER rounding to one decimal, not
/// before, so a size just under a boundary reads as `1.0 MiB` rather than the
/// arithmetically correct but jarring `1024.0 KiB`.
pub fn format_bytes(bytes: u64) -> String {
    /// Where one decimal place starts rounding up to `1024.0`, which is the
    /// point at which the next unit is the honest one.
    const PROMOTE_AT: f64 = 1023.95;
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];

    let mut value = bytes as f64;
    if value < PROMOTE_AT {
        return format!("{bytes} B");
    }
    let mut unit = UNITS[0];
    for next in UNITS {
        unit = next;
        value /= 1024.0;
        if value < PROMOTE_AT {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

/// A count with thousands separators, so a five-figure token count is legible
/// at a glance.
pub fn format_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A count with its noun, singular where the count is one.
fn pluralize(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", format_thousands(count))
    }
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

        // The grouped view is what a person actually reads, so the ranking has
        // to survive grouping. This fixture does not separate the group-order
        // rule from ordering by subtotal; the two `stays_the_first_row` tests
        // below are the ones built to tell those apart.
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

        // The original size of an overwritten result is not recoverable, so the
        // note must not carry one. Without this, a later edit could append a
        // guessed size and every assertion above would still pass.
        for invented in ["B", "KiB", "MiB", "GiB"] {
            assert!(
                !note.contains(invented),
                "the note must not state a size it cannot know: {note}"
            );
        }

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

    /// The group order is the one decision that separates "what ate my
    /// context" from "which server did the most work". A server with three
    /// medium tools out-subtotals a server with one huge one, so ordering
    /// groups by subtotal would bury the heaviest tool below three lighter
    /// ones. This fixture is built so the two rules disagree.
    #[test]
    fn the_heaviest_tool_stays_the_first_row_even_when_another_server_out_subtotals_it() {
        let rows = vec![
            row("fetch_page", Some("web"), 1, 40_000, 10_000),
            row("read_file", Some("fileio"), 1, 16_000, 4_000),
            row("list_dir", Some("fileio"), 1, 16_000, 4_000),
            row("stat_path", Some("fileio"), 1, 16_000, 4_000),
        ];

        let groups = group_by_namespace(&rows, SortAxis::Tokens);
        assert_eq!(
            groups[0].key,
            NamespaceKey::Named("web".into()),
            "the group holding the heaviest tool comes first"
        );
        assert_eq!(
            flat_names(&groups)[0],
            "fetch_page",
            "the heaviest tool is the first row on screen"
        );
        assert!(
            groups[1].subtotal_tokens > groups[0].subtotal_tokens,
            "the fixture only proves the rule if the second group out-subtotals the first: \
             {} vs {}",
            groups[1].subtotal_tokens,
            groups[0].subtotal_tokens
        );
    }

    /// Same shape on the other axis: a chatty server out-subtotals the single
    /// chattiest tool, and the chattiest tool still tops the view.
    #[test]
    fn the_chattiest_tool_stays_the_first_row_even_when_another_server_out_subtotals_it() {
        let rows = vec![
            row("fetch_page", Some("web"), 30, 4_000, 1_000),
            row("read_file", Some("fileio"), 12, 4_000, 1_000),
            row("list_dir", Some("fileio"), 12, 4_000, 1_000),
            row("stat_path", Some("fileio"), 12, 4_000, 1_000),
        ];

        let groups = group_by_namespace(&rows, SortAxis::Calls);
        assert_eq!(flat_names(&groups)[0], "fetch_page");
        assert!(
            groups[1].subtotal_calls > groups[0].subtotal_calls,
            "the fixture only proves the rule if the second group out-subtotals the first: \
             {} vs {}",
            groups[1].subtotal_calls,
            groups[0].subtotal_calls
        );
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
        assert_eq!(format_bytes(1_023), "1023 B");
        assert_eq!(format_bytes(1_024), "1.0 KiB");
    }

    /// A size just under a unit boundary rounds up to `1024.0` at one decimal.
    /// The next unit is the honest reading, and the figure must not be printed
    /// in a unit it has already outgrown.
    #[test]
    fn format_bytes_promotes_the_unit_at_a_rounding_boundary() {
        assert_eq!(format_bytes(1_048_575), "1.0 MiB");
        assert_eq!(format_bytes(1_073_741_823), "1.0 GiB");
        assert_eq!(format_bytes(u64::MAX), "17179869184.0 GiB");
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
