//! Acceptance tier. Every `#[test]` here drives the `rqmd` binary end-to-end.
//! Naming: `ac_<N>_<behaviour>`, where N is the acceptance criterion in the
//! linked issue. Enforced by scripts/lint-acceptance-tests.sh, not at runtime.
mod ast_languages;
mod collection;
mod embed_reuse;
mod exclusion_visibility;
mod graceful_interrupt;
mod helpers;
mod hook_signals;
mod index_rebuild;
mod output_plain;
mod query_timeout;
mod security;
mod status_cost;
mod update_skip;
mod walk_filters;
mod watch;
