//! Acceptance tier. Every `#[test]` here drives the `rqmd` binary end-to-end.
//! Naming: `ac_<N>_<behaviour>`, where N is the acceptance criterion in the
//! linked issue. Enforced by scripts/lint-acceptance-tests.sh, not at runtime.
mod collection;
mod helpers;
