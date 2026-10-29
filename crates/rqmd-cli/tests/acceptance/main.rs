//! Acceptance tier. Every `#[test]` here drives the `rqmd` binary end-to-end.
//! Naming: `ac_<N>_<behaviour>`, where N is the acceptance criterion in the
//! linked issue. Enforced by scripts/lint-acceptance-tests.sh, not at runtime.
mod ast_languages;
mod collection;
mod daemon_idle_exit;
mod embed_exclusion;
mod embed_reuse;
mod eval_judgements;
mod exclusion_visibility;
mod fts_exact_delete;
mod graceful_interrupt;
mod helpers;
mod hook_signals;
mod index_rebuild;
mod mcp_refresh;
mod mcp_stop_force;
mod multi_get;
mod output_plain;
mod per_file;
mod query_timeout;
mod query_timings;
mod read_no_index;
mod recency;
mod security;
mod status_cost;
mod update_skip;
mod walk_filters;
mod watch;
