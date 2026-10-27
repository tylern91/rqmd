# Research notes

[← README](../../README.md)

One-off investigation writeups that don't belong in the standing docs
(`ARCHITECTURE.md`, `CLI.md`, `MIGRATING.md`) but are worth keeping for the
next person who hits the same question.

| Date | Doc | Summary |
|------|-----|---------|
| 2026-10-27 | [external-collection-paths.md](2026-10-27-external-collection-paths.md) | Threat model for a project-local `.rqmd/` index whose collections point outside the project, the options compared, and the decision: warn for one release, then skip by default behind `--allow-external-paths` (design #148). |
| 2026-10-24 | [issue-triage-and-dependency-review.md](2026-10-24-issue-triage-and-dependency-review.md) | Triage of 85 externally reported search and indexing problems against rqmd, the dependency and advisory review (`cargo audit`, soak waivers, llama-cpp-2 skip) and the CI findings behind the 2026-10 CI changes. |
| 2026-10-13 | [performance-adoption.md](2026-10-13-performance-adoption.md) | Performance and quality changes shipped in 0.24.0 to 0.27.0, ideas filed as issues, and a measured quality and speed comparison of EmbeddingGemma-2 with the current model (not adopted). |
| 2026-10-04 | [issue-decisions.md](2026-10-04-issue-decisions.md) | Why #97 (filtered HNSW search), #100 (HNSW compaction), #105 (remote backend) and #106 (daemon model sharing) were closed `not_planned`, with the benchmark and source evidence. |
| 2026-09-27 | [adoption-assessment.md](2026-09-27-adoption-assessment.md) | Comparison of 54 externally reported search and indexing problems against rqmd, plus a from-scratch security audit of the four rqmd crates. Four security bugs and two correctness bugs shipped; twelve adoption candidates and five lower-severity security findings tracked as GitHub issues. |
| 2026-09-23 | [atdd-migration-index_rebuild-cluster.md](2026-09-23-atdd-migration-index_rebuild-cluster.md) | ATDD verdict table for the `index_rebuild` acceptance-test cluster (issue #66's scoped-rebuild regression). |
