//! Context-window fill view, now shared via `client-ui-common`. Re-exported here
//! so existing `crate::context_usage::…` paths (and the executor's CSS-class
//! application via `ContextFillLevel::css_class`) keep resolving unchanged.

pub use client_ui_common::ContextFillLevel;
