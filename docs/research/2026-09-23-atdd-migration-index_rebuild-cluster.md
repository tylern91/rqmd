# ATDD migration — `index_rebuild` cluster

Date: 2026-09-23
Cluster: `crates/rqmd-cli/tests/acceptance/index_rebuild.rs`

## Context

This cluster is not a fold-in of pre-existing unit tests — there is no prior test suite covering
scoped `embed -c <collection> --rebuild`. It is a brand-new acceptance test written to pin the #66
regression (unconditional `hnsw.usearch` deletion corrupting every other collection's vector
search) as part of the same PR that fixes it.

## Verdict table

| Test | Verdict | Reason |
|---|---|---|
| `ac_1_scoped_rebuild_preserves_other_collections_vectors` | **New** | No prior test exists to fold, keep, or delete. Drives the real `rqmd` binary through `collection add` → `update` → `embed` → `embed -c a --rebuild` → `query`/`vsearch`/`doctor`, proving collection `b`'s vector search survives a scoped rebuild of collection `a`. |

## Related unit coverage — not part of this cluster

The shared-hash `content_vectors` SQL defect (the second, independent bug found in the same code
path — a naive `collection = ?1` delete also wiping vectors for a hash shared with another active
collection) is covered separately by an inline unit test,
`scoped_rebuild_clear_keeps_hash_shared_with_another_collection`, in
`crates/rqmd-core/src/db.rs`. This is a deliberate **Keep**-shaped addition at the unit tier, not a
fold: the acceptance test above exercises the CLI end-to-end and cannot precisely isolate the SQL
predicate's shared-hash exclusion, so the unit test guards a distinct branch the acceptance test
does not reach directly. Both were verified via mutation-and-revert evidence this session.
