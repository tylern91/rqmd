use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use rqmd_core::embed_exclusion::{self, EmbedExclusions, ExcludedDocs};
use rqmd_core::{
    Collection, Deadline, Document, IndexLock, IndexOutcome, PendingVectorMeta, TimedOut, db,
    snap_char_boundary_backward,
};

use crate::exit::{Interrupted, PartialFailure};
use crate::hook::{self, HookConfig, HookOutcome};
use crate::{document, exclusions, format as fmt, signals, store};

pub fn run_status(index_dir: &Path) -> Result<()> {
    if store::report_if_no_index(index_dir) {
        return Ok(());
    }
    let s = store::open_store_no_backend(index_dir, true)?;

    // ── Index size (single combined line, mirroring qmd's `Size:`) ──────────────
    let db_size = std::fs::metadata(index_dir.join("index.sqlite"))
        .map(|m| m.len())
        .unwrap_or(0);
    let tantivy_size: u64 = dir_size(&index_dir.join("tantivy"));
    let hnsw_size = std::fs::metadata(index_dir.join("hnsw.usearch"))
        .map(|m| m.len())
        .unwrap_or(0);
    let total_index_size = db_size + tantivy_size + hnsw_size;

    // ── Document counts ──────────────────────────────────────────────────────────
    let total_docs: i64 =
        s.db.query_row("SELECT COUNT(*) FROM documents WHERE active=1", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let total_vecs: i64 =
        s.db.query_row("SELECT COUNT(*) FROM content_vectors", [], |r| r.get(0))
            .unwrap_or(0);
    let (exclusions, excluded) = load_exclusions(&s.db);
    let docs_needing_embed: i64 = db::count_docs_needing_embed_excluding(
        &s.db,
        &store::expected_fingerprint(),
        &store::expected_ast_fingerprints(),
        &excluded.ids,
    )
    .unwrap_or(0);
    let last_modified: Option<String> =
        s.db.query_row(
            "SELECT MAX(modified_at) FROM documents WHERE active=1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(None);

    // ── Header (qmd.ts:492 style, rqmd branding) ────────────────────────────────
    let p = fmt::Paint::stdout();
    println!("{}\n", p.bold("RQMD Status (Rust engine)"));
    println!("Index: {}", index_dir.display());
    println!("Size:  {}", fmt_bytes(total_index_size));
    println!();

    // ── Documents (qmd.ts:513-521) ───────────────────────────────────────────────
    println!("{}", p.bold("Documents"));
    println!("  Total:    {total_docs} files indexed");
    println!("  Vectors:  {total_vecs} embedded");
    if docs_needing_embed > 0 {
        println!(
            "  {} (run 'rqmd embed')",
            p.yellow(format!("Pending:  {docs_needing_embed} need embedding"))
        );
    }
    // Only when a collection has an embed rule, so default output is unchanged.
    if !exclusions.is_empty() {
        println!("  Excluded: {}", describe_excluded(&excluded));
    }
    if let Some(ref ts) = last_modified {
        println!("  Updated:  {}", fmt::format_time_ago(ts));
    }
    store::warn_if_raw_backfill_pending(&s);

    // ── AST Chunking (qmd.ts:539-563) ─────────────────────────────────────────────
    println!("\n{}", p.bold("AST Chunking"));
    if rqmd_core::chunking::ast_chunking_compiled() {
        let exts = rqmd_core::chunking::ast_chunking_extensions().join(", ");
        println!("  Status:   {}", p.green("enabled"));
        println!("  Grammars: {exts}");
        let without = extensions_without_grammar(
            &db::active_extension_counts(&s.db).unwrap_or_default(),
            &rqmd_core::chunking::ast_chunking_extensions(),
        );
        if !without.is_empty() {
            println!("  Without:  {} (no grammar)", p.dim(without));
        }
    } else {
        println!(
            "  Status:   {} (build with --features ast-chunking)",
            p.dim("not available")
        );
    }

    // ── Collections (qmd.ts:565-586, per-collection multi-line blocks) ───────────
    let cols = db::list_collections(&s.db)?;
    print_status_collections(&s.db, &cols)?;

    // ── Models (qmd.ts:606-617) — show repo (browsable) + exact downloaded file ──
    println!("\n{}", p.bold("Models"));
    println!(
        "  Embedding:   https://huggingface.co/{}",
        rqmd_llm::DEFAULT_EMBED_REPO
    );
    println!(
        "               {}",
        p.dim(format!("└─ {}", rqmd_llm::DEFAULT_EMBED_FILE))
    );
    println!(
        "  Reranking:   https://huggingface.co/{}",
        rqmd_llm::DEFAULT_RERANK_REPO
    );
    println!(
        "               {}",
        p.dim(format!("└─ {}", rqmd_llm::DEFAULT_RERANK_FILE))
    );
    println!(
        "  Generation:  https://huggingface.co/{}",
        rqmd_llm::DEFAULT_GENERATE_REPO
    );
    println!(
        "               {}",
        p.dim(format!("└─ {}", rqmd_llm::DEFAULT_GENERATE_FILE))
    );

    // ── Tips (qmd.ts:621-654) ────────────────────────────────────────────────────
    print_status_tips(&s.db, &cols);

    Ok(())
}

/// The stored embed-exclusion rules (#147) and the documents they keep out of
/// `rqmd embed`. A report command shows nothing excluded rather than failing
/// when they cannot be read.
fn load_exclusions(conn: &Connection) -> (EmbedExclusions, ExcludedDocs) {
    let exclusions = EmbedExclusions::load(conn).unwrap_or_default();
    let excluded = embed_exclusion::excluded_docs(conn, &exclusions).unwrap_or_default();
    (exclusions, excluded)
}

/// `N from embedding (A by rule, B over the size cap)`, shared by `rqmd
/// status` and `rqmd doctor`.
fn describe_excluded(excluded: &ExcludedDocs) -> String {
    format!(
        "{} from embedding ({} by rule, {} over the size cap)",
        excluded.total(),
        excluded.by_rule,
        excluded.by_size
    )
}

/// Print the "Collections" and "Examples" blocks of `rqmd status`
/// (qmd.ts:565-601).
fn print_status_collections(conn: &Connection, cols: &[Collection]) -> Result<()> {
    let p = fmt::Paint::stdout();
    if cols.is_empty() {
        println!(
            "\n{}",
            p.dim("No collections. Run 'rqmd collection add .' to index markdown files.")
        );
        return Ok(());
    }

    println!("\n{}", p.bold("Collections"));
    for col in cols {
        let (count, last_mod) = db::collection_doc_stats(conn, &col.name).unwrap_or((0, None));
        let last_mod_str = last_mod
            .as_deref()
            .map(fmt::format_time_ago)
            .unwrap_or_else(|| "never".to_string());
        println!(
            "  {} {}",
            p.cyan(&col.name),
            p.dim(format!("(rqmd://{}/)", col.name))
        );
        println!("    {}  {}", p.dim("Pattern:"), col.pattern);
        println!(
            "    {}    {count} (updated {last_mod_str})",
            p.dim("Files:")
        );
        if let Ok(Some(ctx)) = db::get_context_for_collection(conn, &col.name) {
            let preview = truncate_context_preview(&ctx);
            println!("    {} 1", p.dim("Contexts:"));
            println!("      {} {preview}", p.dim("/:"));
        }
    }

    // ── Examples (qmd.ts:588-601, using rqmd command names) ─────────────────
    println!("\n{}", p.bold("Examples"));
    println!("  {}", p.dim("# List files in a collection"));
    if let Some(first) = cols.first() {
        println!("  rqmd ls {}", first.name);
    }
    println!("  {}", p.dim("# Get a document"));
    if let Some(first) = cols.first() {
        println!("  rqmd get rqmd://{}/path/to/file.md", first.name);
    }
    println!("  {}", p.dim("# Search within a collection"));
    if let Some(first) = cols.first() {
        println!("  rqmd search \"query\" -c {}", first.name);
    }
    Ok(())
}

/// Build and print the "Tips" block of `rqmd status` (qmd.ts:621-654).
fn print_status_tips(conn: &Connection, cols: &[Collection]) {
    let p = fmt::Paint::stdout();
    let mut tips: Vec<String> = Vec::new();

    // Tip 1: collections missing context
    let without_ctx: Vec<&str> = cols
        .iter()
        .filter(|c| {
            db::get_context_for_collection(conn, &c.name)
                .ok()
                .flatten()
                .is_none()
        })
        .map(|c| c.name.as_str())
        .collect();
    if !without_ctx.is_empty() {
        let names = without_ctx[..without_ctx.len().min(3)].join(", ");
        let more = if without_ctx.len() > 3 {
            format!(" +{} more", without_ctx.len() - 3)
        } else {
            String::new()
        };
        tips.push(format!(
            "Add context to collections for clearer `rqmd get`/`rqmd search` output: {names}{more}"
        ));
        tips.push(format!(
            "  {}",
            p.dim("rqmd context add rqmd://<name>/ \"What this collection contains\"")
        ));
    }

    // Tip 2: collections missing update_command (only when >1 collection)
    if cols.len() > 1 {
        let without_update: Vec<&str> = cols
            .iter()
            .filter(|c| c.update_command.is_none())
            .map(|c| c.name.as_str())
            .collect();
        if !without_update.is_empty() {
            let names = without_update[..without_update.len().min(3)].join(", ");
            let more = if without_update.len() > 3 {
                format!(" +{} more", without_update.len() - 3)
            } else {
                String::new()
            };
            tips.push(format!(
                "Add update commands to keep collections fresh: {names}{more}"
            ));
            tips.push(format!(
                "  {}",
                p.dim("rqmd collection update-cmd <name> 'git pull --rebase --ff-only'")
            ));
        }
    }

    if !tips.is_empty() {
        println!("\n{}", p.bold("Tips"));
        for tip in &tips {
            println!("  {tip}");
        }
    }
}

/// Flush the HNSW file to disk, then atomically commit buffered vector metadata
/// rows to SQLite.  Called every CHECKPOINT_INTERVAL docs and at the end of embed.
///
/// Ordering guarantee: the HNSW save must succeed before any DB rows are written.
/// If interrupted between the two steps, only the HNSW is updated — the next run
/// will re-embed the un-written docs, producing new vids that continue from
/// `index.size()` (set by VectorIndex::load → next_vid = size).
///
/// `pending_deletes` holds hashes whose stale (superseded) `content_vectors` rows
/// must be dropped. The delete runs inside the same transaction as the new rows'
/// upsert, and *before* it — `upsert_vector_meta`'s `ON CONFLICT(hash, seq) DO
/// UPDATE` would otherwise just update the stale row in place instead of the
/// delete-then-insert swap this depends on. The corresponding HNSW vids must
/// already have been evicted from the in-memory index by the caller (via
/// `Store::evict_hnsw_vectors`, using vids captured before this deletes the DB's
/// only record of them) — that eviction is persisted by the `s.flush()` below.
fn checkpoint(
    s: &mut rqmd_core::Store,
    pending: &mut Vec<PendingVectorMeta>,
    pending_deletes: &mut HashSet<String>,
) -> Result<()> {
    if pending.is_empty() && pending_deletes.is_empty() {
        return Ok(());
    }
    // 1. Persist HNSW first — this is the durability barrier.
    s.flush()?;
    // Capture the allocator's high-water-mark before opening the transaction — `next_vid`
    // reads through `s`, which the transaction below borrows mutably.
    let next_vid = s.next_vid();
    // 2. Write metadata rows in a single transaction.
    let tx = s.db.transaction()?;
    for hash in pending_deletes.drain() {
        db::delete_vectors_for_hash(&tx, &hash).context("delete stale vectors for hash")?;
    }
    let mut embedded_hashes: HashSet<String> = HashSet::new();
    for m in pending.drain(..) {
        if embedded_hashes.insert(m.hash.clone()) {
            // The new text has its own vectors now, so the version it replaced
            // no longer needs to survive as a donor.
            db::clear_prev_hash(&tx, &m.hash).context("clear superseded hash marker")?;
        }
        db::upsert_vector_meta(
            &tx,
            &m.hash,
            m.seq,
            m.pos,
            &m.model,
            &m.fingerprint,
            m.total_chunks,
            m.vid,
            &m.now,
        )
        .context("upsert vector meta")?;
        db::set_vector_identity(&tx, &m.hash, m.seq, &m.chunk_hash, &m.vector_key)
            .context("record chunk identity")?;
    }
    // Persist the true historical high-water-mark next_vid, atomically with the metadata
    // above — unlike MAX(content_vectors.vid), this floor survives hard-deleted rows, so
    // `Store::open`'s reconciliation never reissues a vid that once existed. Uses the
    // floor-defending helper rather than a blind overwrite (the `3a67f85`/#62 regression
    // class).
    db::persist_next_vid_floor(&tx, next_vid).context("persist next_vid")?;
    tx.commit().context("commit vector metadata")?;
    s.vectors_committed();
    Ok(())
}

/// How many documents to embed before checkpointing HNSW+DB.
/// Lower = more frequent saves (better resume granularity), higher = faster.
const CHECKPOINT_INTERVAL: usize = 50;

/// Render one `rqmd embed` TTY progress line: bar, percent, chunk/doc counts,
/// throughput, and ETA. Throughput/ETA stay placeholders until `elapsed_secs`
/// passes 2s and at least one doc is done, matching the original inline logic.
fn render_embed_progress_line(
    done: usize,
    todo_total: usize,
    chunks_so_far: usize,
    bytes_processed: usize,
    elapsed_secs: f64,
    p: fmt::Paint,
    plain: bool,
) -> String {
    let pct = if todo_total > 0 {
        (done as f64 / todo_total as f64) * 100.0
    } else {
        100.0
    };
    let bar = if plain {
        String::new()
    } else {
        format!("{} ", p.cyan(fmt::render_progress_bar(pct, 30)))
    };
    let pct_int = pct.round() as u64;
    let (throughput_str, eta_str) = if elapsed_secs > 2.0 && done > 0 {
        let bps = bytes_processed as f64 / elapsed_secs;
        let docs_per_sec = done as f64 / elapsed_secs;
        let remaining = (todo_total - done) as f64 / docs_per_sec.max(0.001);
        (
            format!("{}/s", fmt_bytes(bps as u64)),
            fmt::format_eta(remaining),
        )
    } else {
        (".../s".to_string(), "...".to_string())
    };
    format!(
        "{bar}{} {}",
        p.bold(format!("{pct_int:>3}% input")),
        p.dim(format!(
            "{chunks_so_far} chunks · {done}/{todo_total} docs · {throughput_str} · ETA {eta_str}"
        ))
    )
}

/// Documents in a row that may fail to embed before the run stops: that many
/// consecutive failures point at the model or GPU, not at one bad document.
const MAX_CONSECUTIVE_EMBED_FAILURES: usize = 5;

/// Failed documents listed in the end-of-run summary before it elides the rest.
const MAX_LISTED_EMBED_FAILURES: usize = 20;

struct EmbedFailure {
    collection: String,
    path: String,
    error: String,
}

/// State shared by every collection's pass of the embed loop.
struct EmbedRun {
    /// Vector metadata staged for the next checkpoint.
    pending: Vec<PendingVectorMeta>,
    /// Hashes whose superseded `content_vectors` rows are dropped at the next
    /// checkpoint (see `checkpoint`).
    pending_deletes: HashSet<String>,
    failures: Vec<EmbedFailure>,
    consecutive_failures: usize,
    new_docs: usize,
    new_chunks: usize,
    checkpoint_interval: usize,
}

impl EmbedRun {
    fn new(checkpoint_interval: usize) -> Self {
        Self {
            pending: Vec::new(),
            pending_deletes: HashSet::new(),
            failures: Vec::new(),
            consecutive_failures: 0,
            new_docs: 0,
            new_chunks: 0,
            checkpoint_interval,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum EmbedFlow {
    Continue,
    Abort,
    TimedOut(TimedOut),
    /// A signal asked the run to stop; carries its number.
    Interrupted(i32),
}

/// Embed `docs` in order. A document whose embedding fails is recorded in
/// `run.failures` and skipped, leaving its existing vectors untouched; only
/// after a document embeds successfully are its superseded vectors evicted and
/// queued for deletion — in the same iteration, so a checkpoint triggered by
/// this document already covers them. Returns [`EmbedFlow::Abort`] once
/// [`MAX_CONSECUTIVE_EMBED_FAILURES`] documents fail in a row, and
/// [`EmbedFlow::TimedOut`] when `deadline` passes before the next document
/// starts, and [`EmbedFlow::Interrupted`] when `interrupted` reports a signal
/// before the next document starts, so the document in hand always finishes.
/// Errors from the index itself (not from embedding) are returned as `Err`.
fn embed_docs(
    s: &mut rqmd_core::Store,
    run: &mut EmbedRun,
    docs: &[&Document],
    deadline: Deadline,
    interrupted: &dyn Fn() -> Option<i32>,
    mut on_progress: impl FnMut(usize, usize, usize),
) -> Result<EmbedFlow> {
    let mut done = 0usize;
    let mut bytes_processed = 0usize;
    for doc in docs {
        if let Some(signal) = interrupted() {
            return Ok(EmbedFlow::Interrupted(signal));
        }
        if let Err(timed_out) = deadline.check("the next document") {
            return Ok(EmbedFlow::TimedOut(timed_out));
        }
        let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
        if body.is_empty() {
            db::clear_prev_hash(&s.db, &doc.hash)?;
            continue;
        }
        on_progress(done, bytes_processed, run.new_chunks + run.pending.len());

        match s.embed_document_chunks(&doc.hash, &doc.path, &body) {
            Ok(new_chunks) => {
                if db::hash_has_any_vector(&s.db, &doc.hash) {
                    let stale_vids = db::vids_for_hash(&s.db, &doc.hash)?;
                    s.evict_hnsw_vectors(&stale_vids)?;
                    run.pending_deletes.insert(doc.hash.clone());
                }
                run.consecutive_failures = 0;
                run.new_chunks += new_chunks.len();
                run.new_docs += 1;
                run.pending.extend(new_chunks);
                done += 1;
                bytes_processed += body.len();

                // Checkpoint every N docs so an interrupt only re-embeds the last batch.
                if done.is_multiple_of(run.checkpoint_interval) {
                    checkpoint(s, &mut run.pending, &mut run.pending_deletes)?;
                }
            }
            Err(e) => {
                run.consecutive_failures += 1;
                run.failures.push(EmbedFailure {
                    collection: doc.collection.clone(),
                    path: doc.path.clone(),
                    error: format!("{e:#}"),
                });
                if run.consecutive_failures >= MAX_CONSECUTIVE_EMBED_FAILURES {
                    return Ok(EmbedFlow::Abort);
                }
            }
        }
    }
    Ok(EmbedFlow::Continue)
}

const EMBED_RESUME_HINT: &str = "Embedded work so far was saved; rerun `rqmd embed` to resume.";

/// Print the skipped documents and turn how the run ended into its exit status.
fn finish_embed(run: &EmbedRun, flow: &EmbedFlow) -> Result<()> {
    if !run.failures.is_empty() {
        eprintln!(
            "\nCould not embed {} document(s); their existing vectors were left as they were:",
            run.failures.len()
        );
        for f in run.failures.iter().take(MAX_LISTED_EMBED_FAILURES) {
            eprintln!("  {}/{}: {}", f.collection, f.path, f.error);
        }
        if run.failures.len() > MAX_LISTED_EMBED_FAILURES {
            eprintln!(
                "  ... and {} more",
                run.failures.len() - MAX_LISTED_EMBED_FAILURES
            );
        }
    }
    match flow {
        EmbedFlow::Abort => anyhow::bail!(
            "stopped after {MAX_CONSECUTIVE_EMBED_FAILURES} consecutive embed failures \
             (likely a model or GPU problem); embedded work was saved — fix the cause and rerun to resume"
        ),
        EmbedFlow::TimedOut(timed_out) => {
            eprintln!("{EMBED_RESUME_HINT}");
            Err((*timed_out).into())
        }
        EmbedFlow::Interrupted(signal) => {
            eprintln!("{EMBED_RESUME_HINT}");
            Err(Interrupted(*signal).into())
        }
        EmbedFlow::Continue if run.failures.is_empty() => Ok(()),
        EmbedFlow::Continue => Err(PartialFailure(format!(
            "{} document(s) could not be embedded",
            run.failures.len()
        ))
        .into()),
    }
}

pub fn run_embed(
    index_dir: &Path,
    collection: Option<&str>,
    rebuild: bool,
    timeout: Option<Duration>,
) -> Result<()> {
    // Held for the whole command: `Store::open`'s next_vid floor and the
    // HNSW file are both unguarded against a second concurrent writer.
    let lock = IndexLock::acquire(index_dir)?;
    // A signal stops the embed loop after the document in hand; a second ends the process.
    let _catch = signals::Catch::install_exiting_on_repeat();

    let cols = {
        let s = store::open_store_no_backend(index_dir, true)?;
        match collection {
            Some(c) => vec![
                db::list_collections(&s.db)?
                    .into_iter()
                    .find(|col| col.name == c)
                    .with_context(|| format!("collection '{c}' not found"))?,
            ],
            None => db::list_collections(&s.db)?,
        }
    };

    if cols.is_empty() {
        println!("No collections to embed.");
        return Ok(());
    }

    // --rebuild: clear the vector index and re-embed everything from scratch.
    // Delete the HNSW file and all content_vectors rows *before* opening the backend
    // so that Store::open starts with a clean slate (next_vid=0, no DB vids).
    if rebuild {
        match collection {
            Some(c) => {
                // Writable handle: evict_hnsw_vectors needs a non-view HNSW,
                // same reason collection::remove opens with read_only=false.
                let mut s = store::open_store_no_backend(index_dir, false)?;
                // Evict before the DELETE below: the vids query reads rows
                // the DELETE removes.
                let vids = db::rebuildable_vids_for_collection(&s.db, c)
                    .context("list rebuildable vids")?;
                s.evict_hnsw_vectors(&vids)
                    .context("evict collection vectors")?;
                db::clear_vectors_for_collection(&s.db, c)
                    .context("clear vectors for collection")?;
                s.flush().context("flush after scoped rebuild eviction")?;
            }
            None => {
                // Unscoped: content_vectors is content-addressed, shared across
                // every collection, so dropping the whole HNSW file is correct
                // (and cheaper than evicting every vid one at a time) only when
                // every collection's vectors are being cleared together. Doing
                // this for a *scoped* rebuild would silently break vector
                // search for every other collection sharing the file — see #66.
                let hnsw_path = store::store_config(index_dir, true).hnsw_path;
                if hnsw_path.exists() {
                    std::fs::remove_file(&hnsw_path)
                        .with_context(|| format!("remove hnsw file: {}", hnsw_path.display()))?;
                }
                let s = store::open_store_no_backend(index_dir, true)?;
                db::clear_all_vectors(&s.db).context("clear all vectors")?;
            }
        }
        eprintln!(
            "{}",
            fmt::Paint::stderr().yellow(format!(
                "rqmd: rebuild mode — cleared {} vectors; re-embedding from scratch",
                if collection.is_some() {
                    "collection"
                } else {
                    "all"
                }
            ))
        );
    } else {
        // Fast path: nothing to do.
        let s = store::open_store_no_backend(index_dir, true)?;
        // Embed-excluded documents (#147) are not pending, so a collection whose
        // only unembedded documents are excluded still takes this path.
        let needs_embed: i64 = embed_exclusion::pending_embed_count(
            &s.db,
            &store::expected_fingerprint(),
            &store::expected_ast_fingerprints(),
        )
        .unwrap_or(1);
        if needs_embed == 0 {
            println!(
                "{}",
                fmt::Paint::stdout().green("✓ All content hashes already have embeddings.")
            );
            return Ok(());
        }
    }

    let s = store::open_store_with_backend(index_dir, false)?;
    embed_collections(s, &cols, rebuild, timeout, &lock)
}

/// The embed pass over an already-open store: advisories, the per-collection
/// loop, the final checkpoint and the summary.
fn embed_collections(
    mut s: rqmd_core::Store,
    cols: &[Collection],
    rebuild: bool,
    timeout: Option<Duration>,
    lock: &IndexLock,
) -> Result<()> {
    // Stale-fingerprint advisory: --rebuild already cleared every vector row above,
    // so there is nothing to compare yet; only warn on the incremental path, where
    // a model/chunking change leaves existing vectors under the old fingerprint
    // until this run re-embeds them (`hash_needs_embed` selects every hash with no
    // vector at the current fingerprint, not only hashes with no vectors at all).
    if !rebuild {
        store::warn_if_fingerprint_stale(&s);
        s.backfill_vector_keys()
            .context("fill in reuse keys of stored vectors")?;
    }

    // Advisory: detect when the HNSW index is smaller than what the DB references.
    // next_vid reconciliation (Store::open) prevents the UNIQUE crash; this warning
    // surfaces latent missing-vector gaps that only --rebuild can fully repair.
    {
        let hnsw_size = s.hnsw_size() as i64;
        let db_vec_count: i64 =
            s.db.query_row("SELECT COUNT(*) FROM content_vectors", [], |r| r.get(0))
                .unwrap_or(0);
        if hnsw_size < db_vec_count {
            eprintln!(
                "{}",
                fmt::Paint::stderr().yellow(format!(
                    "rqmd: warning: vector index out of sync ({hnsw_size} indexed \
                     vs {db_vec_count} expected); run `rqmd embed --rebuild` to repair."
                ))
            );
        }
    }
    let mut progress = fmt::Progress::new(fmt::ProgressMode::detect());
    let start = Instant::now();

    // Pending vector metadata is flushed every CHECKPOINT_INTERVAL docs.
    let mut run = EmbedRun::new(CHECKPOINT_INTERVAL);
    let mut end = EmbedFlow::Continue;
    // Starts once the store is open, so index and model load are not charged.
    let deadline = timeout.map_or(Deadline::none(), Deadline::after);

    // Track hashes queued in this run to prevent duplicate-hash drift: multiple documents
    // with identical bodies share a hash, and embedding each copy adds a vector to HNSW
    // while the DB ON-CONFLICT UPDATE overwrites the vid — orphaning the previous vid and
    // widening the HNSW/DB gap on every run.  Deduping by hash here stops that at source.
    let mut seen_hashes: HashSet<String> = HashSet::new();

    // Documents an embed-exclusion rule keeps out (#147), taken over every
    // collection: a hash shared across collections is judged by the documents
    // that may be embedded, the same set `count_docs_needing_embed_excluding`
    // counts. Empty when no collection has a rule.
    let exclusions = EmbedExclusions::load(&s.db)?;
    let excluded = embed_exclusion::excluded_docs(&s.db, &exclusions)?;

    for col in cols {
        // Collect all docs for this collection.  We embed only those whose content
        // hash has no vector row at the current fingerprint (incremental / resumable;
        // also catches hashes whose only vectors are stale, so a chunker/model change
        // gets re-embedded instead of silently skipped).
        let docs = db::list_documents(&s.db, Some(&col.name))?;

        // Collect only docs whose hash still needs embedding per the same
        // hash-level "satisfied by either route" rule `count_docs_needing_embed`
        // uses (not a per-document fingerprint match — see `hash_needs_embed`'s
        // doc comment for why the two must agree), and whose hash has not
        // already been queued in this run (duplicate-hash guard).
        let base_fp = store::expected_fingerprint();
        let ast_fps = store::expected_ast_fingerprints();
        let mut todo_indices: Vec<usize> = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            if excluded.ids.contains(&doc.id) || seen_hashes.contains(&doc.hash) {
                continue;
            }
            if db::hash_needs_embed_excluding(&s.db, &doc.hash, &base_fp, &ast_fps, &excluded.ids)
                .unwrap_or(true)
            {
                seen_hashes.insert(doc.hash.clone());
                todo_indices.push(i);
            }
        }

        let todo_total = todo_indices.len();
        if todo_total == 0 {
            continue;
        }

        let todo: Vec<&Document> = todo_indices.iter().map(|i| &docs[*i]).collect();
        let flow = embed_docs(
            &mut s,
            &mut run,
            &todo,
            deadline,
            &signals::pending,
            |done, bytes, chunks| {
                lock.heartbeat();
                if progress.is_active() {
                    let line = render_embed_progress_line(
                        done,
                        todo_total,
                        chunks,
                        bytes,
                        start.elapsed().as_secs_f64(),
                        fmt::Paint::stderr(),
                        progress.is_plain(),
                    );
                    progress.show(&line, done, todo_total);
                }
            },
        )?;
        if flow != EmbedFlow::Continue {
            end = flow;
            break;
        }
    }

    // Final 100% bar before the summary line. A plain terminal already got its
    // 100% line when the last document finished.
    if end != EmbedFlow::Continue {
        progress.clear();
    } else if progress.is_active() && !progress.is_plain() {
        let p = fmt::Paint::stderr();
        let bar = fmt::render_progress_bar(100.0, 30);
        let line = format!("{} {}", p.green(bar), p.bold("100% input"));
        progress.show(&line, 1, 1);
    }

    // Final checkpoint for any remaining pending rows.
    checkpoint(&mut s, &mut run.pending, &mut run.pending_deletes)?;
    // `rqmd update` spares the vectors of a version an edit replaced until the
    // edit is embedded; the ones nothing needs any more are dropped here.
    s.reclaim_orphaned_vectors()?;

    // Summary — matches qmd's "✓ Done!" line (qmd.ts:1938).
    let elapsed = fmt::format_eta(start.elapsed().as_secs_f64());
    let p = fmt::Paint::stdout();
    let headline = if matches!(end, EmbedFlow::Interrupted(_)) {
        p.yellow("Interrupted.")
    } else {
        p.green("✓ Done!")
    };
    println!(
        "\n{headline} Embedded {} chunks from {} documents in {}",
        p.bold(run.new_chunks),
        p.bold(run.new_docs),
        p.bold(&elapsed)
    );
    finish_embed(&run, &end)
}

/// `rqmd embed --cleanup`: reclaim space without loading a model or touching
/// the corpus. Three DB-side steps, all reversible only by re-embedding:
/// evict orphaned vectors from HNSW and delete their rows, delete `content`
/// rows no document (active or not) references at all, then `VACUUM` to
/// hand the freed pages back to the filesystem.
pub fn run_cleanup(index_dir: &Path) -> Result<()> {
    let _lock = IndexLock::acquire(index_dir)?;

    let hnsw_path = store::store_config(index_dir, false).hnsw_path;
    let db_path = index_dir.join("index.sqlite");
    let before_hnsw = std::fs::metadata(&hnsw_path).map(|m| m.len()).unwrap_or(0);
    let before_db = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let mut s = store::open_store_no_backend(index_dir, false)?;

    // 1. Sweep orphaned vectors — evict from HNSW first, then drop their rows.
    let swept_vectors = s.reclaim_orphaned_vectors()?;

    // 1b. Vectors a plain `rqmd embed` kept for documents an embed-exclusion
    // rule now leaves out (#147). Nothing to do when no collection has a rule.
    let exclusions = EmbedExclusions::load(&s.db)?;
    let excluded = embed_exclusion::excluded_docs(&s.db, &exclusions)?;
    let swept_excluded = s.reclaim_excluded_vectors(&excluded.ids)?;

    // 2. Delete content referenced by no document at all (active or not).
    let swept_content = db::delete_unreferenced_content(&s.db).context("sweep content")?;

    // 3. Return freed pages to the filesystem. Outside any transaction — VACUUM
    // cannot run inside one.
    s.db.execute_batch("VACUUM")
        .context("vacuum index.sqlite")?;

    let after_hnsw = std::fs::metadata(&hnsw_path).map(|m| m.len()).unwrap_or(0);
    let after_db = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    println!(
        "{} Swept {swept_vectors} orphaned vector(s), {swept_content} unreferenced content row(s).",
        fmt::Paint::stdout().green("✓ Cleanup done.")
    );
    if !exclusions.is_empty() {
        println!("  Swept {swept_excluded} vector(s) kept for documents excluded from embedding.");
    }
    println!(
        "  hnsw.usearch: {} -> {}",
        fmt_bytes(before_hnsw),
        fmt_bytes(after_hnsw)
    );
    println!(
        "  index.sqlite: {} -> {}",
        fmt_bytes(before_db),
        fmt_bytes(after_db)
    );
    Ok(())
}

pub fn run_update(
    index_dir: &Path,
    collection: Option<&str>,
    hooks_allowed: bool,
    full: bool,
) -> Result<()> {
    run_update_with(
        index_dir,
        collection,
        hooks_allowed,
        full,
        HookConfig::from_env(),
    )
}

/// `full` re-reads and re-hashes every file instead of trusting the recorded
/// size and mtime of files that look unchanged.
fn run_update_with(
    index_dir: &Path,
    collection: Option<&str>,
    hooks_allowed: bool,
    full: bool,
    hook_config: HookConfig,
) -> Result<()> {
    // Shares IndexLock with run_embed: an `update` mutating `documents` rows
    // mid-way through a concurrent `embed`'s snapshot is a separate hazard
    // from the vid race, but the two commands never need to run at once.
    let lock = IndexLock::acquire(index_dir)?;
    // A signal stops the walk after the file in hand; a second ends the process.
    let _catch = signals::Catch::install_exiting_on_repeat();

    // Re-walk each collection's directory and re-index changed files.
    let cols = {
        let s = store::open_store_no_backend(index_dir, true)?;
        match collection {
            Some(c) => vec![
                db::list_collections(&s.db)?
                    .into_iter()
                    .find(|col| col.name == c)
                    .with_context(|| format!("collection '{c}' not found"))?,
            ],
            None => db::list_collections(&s.db)?,
        }
    };

    if cols.is_empty() {
        println!("No collections to update.");
        return Ok(());
    }

    // Update refreshes BM25 metadata only — no vectors. Run `rqmd embed` afterward
    // to regenerate embeddings. Using the FTS-only store avoids loading the inference
    // backend and prevents content_vectors.vid UNIQUE conflicts on re-indexing.
    // Still needs write access: re-indexing changed/removed files stages FTS
    // writes and DB rows even though flush() now skips hnsw.save() when no
    // vector was added.
    let mut s = store::open_store_no_backend(index_dir, false)?;
    let progress_mode = fmt::ProgressMode::detect();
    let p = fmt::Paint::stdout();

    // Mirror qmd's "Updating N collection(s)..." header (qmd.ts:675).
    println!(
        "{}\n",
        p.bold(format!("Updating {} collection(s)...", cols.len()))
    );

    // Tracks whether every collection was fully walked and every file indexed
    // without error — the raw-split marker must only be set on a clean full
    // pass, otherwise a skipped collection (missing dir, bad glob, 0 matches)
    // or a per-file `prepare`/index failure leaves `documents.raw` at its
    // empty-string default for those rows permanently, since the marker
    // suppresses `warn_if_raw_backfill_pending` forever once set.
    let mut all_collections_clean = true;
    let mut hook_failures: Vec<(String, String)> = Vec::new();

    for (ci, col) in cols.iter().enumerate() {
        if let Some(signal) = signals::pending() {
            return Err(stopped_by_signal(signal));
        }
        // Per-collection header: [i/n] name (pattern)
        println!(
            "{} {} {}",
            p.cyan(format!("[{}/{}]", ci + 1, cols.len())),
            p.bold(&col.name),
            p.dim(format!("({})", col.pattern))
        );

        let outcome = update_one_collection(
            &mut s,
            col,
            UpdateRun {
                progress_mode,
                hooks_allowed,
                full,
                hook_config,
            },
            &|| lock.heartbeat(),
            &signals::pending,
        )?;
        if let Some(report) = &outcome.report {
            println!("{report}");
        }
        if let Some(signal) = outcome.interrupted {
            return Err(stopped_by_signal(signal));
        }
        all_collections_clean &= outcome.clean;
        if let Some(reason) = outcome.hook_failure {
            hook_failures.push((col.name.clone(), reason));
        }
    }

    // A removed document's hash may still be in active use by another document
    // — content is deduplicated globally by hash — so the sweep runs once,
    // globally, after every collection's deactivation pass has landed, rather
    // than per-collection against a partial view of which hashes are still active.
    // A version an edit just replaced is spared until `rqmd embed` has copied
    // its unchanged chunks (`documents.prev_hash`).
    s.reclaim_orphaned_vectors()?;

    // "needs embeddings" notice (qmd.ts:747–748) — printed once after all collections
    // so it isn't repeated N times with the same global count during a multi-collection update.
    let needs_embed: i64 = embed_exclusion::pending_embed_count(
        &s.db,
        &store::expected_fingerprint(),
        &store::expected_ast_fingerprints(),
    )
    .unwrap_or(0);
    if needs_embed > 0 {
        println!(
            "\nRun 'rqmd embed' to update embeddings ({needs_embed} unique hashes need vectors)"
        );
    }

    // Only an unscoped run that touched every document cleanly can certify the
    // raw-split backfill complete — a skipped collection or file leaves some
    // `documents.raw` rows at their empty-string default.
    if collection.is_none() && all_collections_clean {
        store::mark_raw_split_backfilled(&s)?;
    }
    // The rebuild reads document text from `content`, which is trustworthy once
    // a clean pass (this one or an earlier one) has certified the backfill. A
    // later unclean pass — a collection directory gone, one unreadable file —
    // cannot undo that, so it must not block the rebuild forever.
    if collection.is_none() && store::raw_split_backfilled(&s) {
        if s.rebuild_fts_if_needed()? {
            println!("Rebuilt the full-text index for the current tokenizer and schema.");
        }
    } else {
        store::warn_if_fts_rebuild_pending(&s);
    }

    if hook_failures.is_empty() {
        return Ok(());
    }
    eprintln!(
        "\nUpdate hook failed for {} collection(s):",
        hook_failures.len()
    );
    for (name, reason) in &hook_failures {
        eprintln!("  {name}: {reason}");
    }
    Err(PartialFailure(format!(
        "{} update hook(s) failed; all collections were still indexed",
        hook_failures.len()
    ))
    .into())
}

/// The resume hint, then the error that maps to the signal's exit code. What
/// was indexed so far is already saved, and a rerun skips it by stat.
fn stopped_by_signal(signal: i32) -> anyhow::Error {
    eprintln!("Progress was saved; rerun `rqmd update` to continue.");
    Interrupted(signal).into()
}

/// What `update_one_collection` reports back to `run_update`.
pub(super) struct CollectionOutcome {
    /// The collection was fully walked and every file indexed without error.
    pub(super) clean: bool,
    /// Why the collection's update hook failed or timed out, if it did.
    hook_failure: Option<String>,
    /// The signal that stopped this collection's update before it finished.
    pub(super) interrupted: Option<i32>,
    /// The summary line for the caller to print, when the walk got far enough
    /// to have one. Returned, not printed, so a caller whose stdout is a
    /// protocol stream (the MCP server) can drop it.
    pub(super) report: Option<String>,
}

/// Re-walk one collection's directory, run its update hook, re-index changed
/// files, and prune deleted ones. Extracted from `run_update`'s per-collection
/// loop body; the `IndexOutcome`-based tally counters and every WARN-on-failure
/// path are preserved bit-for-bit. `clean` is `false` if any part of the
/// collection was skipped or any file failed to index — the caller uses this
/// to decide whether the run is clean enough to certify the raw-split backfill.
/// A failing or timed-out hook never stops the walk; it is reported in
/// `hook_failure`. When `interrupted` reports a signal the walk stops before
/// the next file, what was indexed is flushed, nothing is pruned, and the
/// signal is returned in `interrupted`.
///
/// `hooks_allowed` gates `col.update_command`: a project-local `.rqmd/` index
/// (picked up implicitly from the current directory) can belong to a repo the
/// current user does not control, so its hook command is skipped unless the
/// caller opted in — see `store::hooks_allowed`.
/// Per-invocation settings shared by every collection in one `rqmd update`.
#[derive(Clone, Copy)]
pub(super) struct UpdateRun {
    pub(super) progress_mode: fmt::ProgressMode,
    pub(super) hooks_allowed: bool,
    pub(super) full: bool,
    pub(super) hook_config: HookConfig,
}

pub(super) fn update_one_collection(
    s: &mut rqmd_core::Store,
    col: &Collection,
    run: UpdateRun,
    heartbeat: &dyn Fn(),
    interrupted: &dyn Fn() -> Option<i32>,
) -> Result<CollectionOutcome> {
    let UpdateRun {
        progress_mode,
        hooks_allowed,
        full,
        hook_config,
    } = run;
    let mut progress = fmt::Progress::new(progress_mode);
    let dir = Path::new(&col.path);
    if !dir.exists() {
        eprintln!("  WARN: directory not found: {}", dir.display());
        return Ok(CollectionOutcome {
            clean: false,
            hook_failure: None,
            interrupted: None,
            report: None,
        });
    }

    // Run the collection's pre-update hook (e.g. `git fetch && git pull ...`)
    // before walking the directory, so indexing sees freshly synced content.
    let mut hook_failure = None;
    if let Some(cmd) = col.update_command.as_deref()
        && !cmd.trim().is_empty()
    {
        if !hooks_allowed {
            eprintln!(
                "  WARN: skipping update hook (project-local index; pass --run-hooks to allow it): {cmd}"
            );
        } else {
            println!("  {}", fmt::Paint::stdout().dim(format!("$ {cmd}")));
            hook_failure = match hook::run_hook(cmd, dir, hook_config) {
                HookOutcome::Succeeded => None,
                HookOutcome::Failed(reason) => Some(reason),
                HookOutcome::TimedOut(limit) => Some(format!(
                    "timed out after {}s and was killed",
                    limit.as_secs()
                )),
                HookOutcome::Interrupted(signal) => {
                    return Ok(CollectionOutcome {
                        clean: false,
                        hook_failure: None,
                        interrupted: Some(signal),
                        report: None,
                    });
                }
            };
            if let Some(reason) = &hook_failure {
                eprintln!("  WARN: update hook {reason}");
            }
        }
    }

    let include_set = match exclusions::build_include_set(&col.pattern) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "  WARN: {}: invalid mask '{}': {e:#}",
                col.name, col.pattern
            );
            return Ok(CollectionOutcome {
                clean: false,
                hook_failure,
                interrupted: None,
                report: None,
            });
        }
    };
    for (pattern, reason) in exclusions::invalid_ignore_patterns(&col.ignore) {
        eprintln!(
            "  WARN: collection '{}': ignoring invalid ignore pattern '{pattern}': {reason}",
            col.name
        );
    }
    let ignore_set = exclusions::build_ignore_set(&col.ignore);

    let mut new_count = 0usize;
    let mut updated_count = 0usize;
    let mut unchanged_count = 0usize;
    let mut processed = 0usize;
    let mut skips = document::SkipCounts::default();
    let mut clean = true;
    let mut stopped_by = None;

    // Pre-collect matching paths so we know the total before indexing begins,
    // enabling "Indexing: N/total" progress (matching qmd's output). Reuses the
    // same walk/filter logic `collection add` uses, so a re-index can't silently
    // disagree with the original add about which files belong in the collection.
    let document::Candidates {
        files,
        too_large: too_large_count,
        excluded_dirs,
    } = document::collect_candidates(
        dir,
        &include_set,
        &ignore_set,
        document::WalkOptions::for_collection(col),
    );
    skips.too_large = too_large_count;
    let total = files.len();
    if total == 0 {
        eprintln!(
            "  WARN: mask '{}' matched 0 files under {} (try --hidden if content lives under a dot-directory)",
            col.pattern,
            dir.display()
        );
        // Not a `clean = false` case: zero matches means zero documents in this
        // collection to backfill, so it can't leave any `documents.raw` row
        // stale — unlike a per-file failure below, which does.
    }

    // Files whose size and mtime still match what they were last indexed from
    // are counted unchanged without being read or hashed. A miss, or `--full`,
    // falls through to the content hash, which stays the source of truth.
    let stat_cache = if full {
        HashMap::new()
    } else {
        db::load_doc_stat_cache(&s.db, &col.name).context("load document stat cache")?
    };

    for path in &files {
        heartbeat();
        if let Some(signal) = interrupted() {
            stopped_by = Some(signal);
            break;
        }
        let stat = document::stat_file(path);
        if let Some(stat) = stat
            && stat_cache.get(&document::rel_path_of(path, dir)) == Some(&stat)
        {
            processed += 1;
            unchanged_count += 1;
            continue;
        }
        let doc = match document::prepare(path, dir) {
            Ok(doc) => doc,
            Err(reason) => {
                skips.record(reason);
                processed += 1;
                clean = false;
                continue;
            }
        };

        processed += 1;
        if progress.is_active() {
            let line = format!("Indexing: {processed}/{total} {}", doc.rel_path);
            progress.show(&line, processed, total);
        }

        match s.index_document_fts_only_with_raw(
            &col.name,
            &doc.rel_path,
            &doc.title,
            &doc.indexed_text,
            &doc.raw,
        ) {
            Err(e) => {
                eprintln!("  WARN: {}: {e:#}", doc.rel_path);
                clean = false;
            }
            Ok(outcome) => {
                match outcome {
                    IndexOutcome::New => new_count += 1,
                    IndexOutcome::Updated => updated_count += 1,
                    IndexOutcome::Unchanged => unchanged_count += 1,
                }
                db::set_document_stat(
                    &s.db,
                    &col.name,
                    &doc.rel_path,
                    stat.and_then(document::trusted_stat),
                )
                .context("record document stat")?;
            }
        }
    }

    // Prune documents whose file is no longer on disk (deleted or renamed away).
    // Built from `files` — the raw walked candidate list — not from the set of
    // docs that made it through `prepare` above: a file that still exists but
    // failed to read this run (permission denied, transient I/O error) is still
    // *present* and must not be soft-deleted just because this pass couldn't
    // read it. Skipped entirely when the mask matched 0 files (warned above) —
    // a config/mount glitch must never read as "delete everything".
    let removed_count = if total > 0 && stopped_by.is_none() {
        let present_paths: HashSet<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(dir)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let removed = db::deactivate_missing_documents(&s.db, &col.name, &present_paths)
            .context("deactivate missing documents")?;
        for path in &removed {
            let filepath = format!("{}/{path}", col.name);
            if let Err(e) = s.remove_from_fts(&filepath) {
                eprintln!("  WARN: failed to remove stale FTS entry for {filepath}: {e:#}");
            }
        }

        removed.len()
    } else {
        0
    };

    s.flush()?;

    progress.clear();

    if stopped_by.is_some() {
        return Ok(CollectionOutcome {
            clean: false,
            hook_failure,
            interrupted: stopped_by,
            report: Some(format!(
                "\nStopped after {processed} of {total} file(s): {new_count} new, {updated_count} updated, {unchanged_count} unchanged"
            )),
        });
    }

    // Summary line: the counts, an honest skip-reason breakdown instead of silently
    // dropping unreadable files, then the folders the built-in rule left out.
    let skip_suffix = if skips.total() > 0 {
        format!(", skipped {} ({})", skips.total(), skips.describe())
    } else {
        String::new()
    };
    let excluded_suffix = document::excluded_dirs_clause(&excluded_dirs)
        .map(|clause| format!(" · {clause}"))
        .unwrap_or_default();
    Ok(CollectionOutcome {
        clean,
        hook_failure,
        interrupted: None,
        report: Some(format!(
            "\nIndexed: {new_count} new, {updated_count} updated, {unchanged_count} unchanged, {removed_count} removed{skip_suffix}{excluded_suffix}"
        )),
    })
}

pub fn run_init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let qmd_dir = cwd.join(".rqmd");

    if qmd_dir.exists() {
        println!("Local index already exists at {}", qmd_dir.display());
        return Ok(());
    }

    std::fs::create_dir_all(&qmd_dir)?;
    // Touch the SQLite db to create it
    let _ = store::open_store_no_backend(&qmd_dir, false)?;
    println!("Initialized local index at {}", qmd_dir.display());
    println!("Run `rqmd collection add <path> --name <name>` to add a collection.");
    Ok(())
}

pub fn run_doctor(index_dir: &Path) -> Result<()> {
    println!("RQMD Doctor (Rust engine)\n");

    let db_path = index_dir.join("index.sqlite");
    println!("  Index dir:     {}", index_dir.display());
    println!(
        "  SQLite exists: {}",
        if db_path.exists() {
            "yes"
        } else {
            "NO — run `rqmd collection add <path>` to create one"
        }
    );
    println!("  Tantivy dir:   {}", index_dir.join("tantivy").display());
    println!(
        "  HNSW file:     {}",
        index_dir.join("hnsw.usearch").display()
    );
    println!();

    // Check models cache — delegate to rqmd-llm so the path and repo IDs always
    // match what the downloader uses, and HF_HOME is honoured.
    let model_report = rqmd_llm::model_cache_report(&rqmd_llm::LlamaCppConfig::default());
    println!("  Model cache:   {}", model_report.cache_root.display());
    println!(
        "  Embed model:   {}",
        if model_report.embed_cached {
            "cached ✓"
        } else {
            "not cached (downloads on first embed/query)"
        }
    );
    println!(
        "  Rerank model:  {}",
        if model_report.rerank_cached {
            "cached ✓"
        } else {
            "not cached"
        }
    );
    println!(
        "  Generation:    {}",
        if model_report.generate_cached {
            "cached ✓"
        } else {
            "not cached (downloads on first model load alongside embed/rerank)"
        }
    );

    // Check GPU
    #[cfg(target_os = "macos")]
    println!("  GPU backend:   Metal (Apple Silicon detected)");
    #[cfg(not(target_os = "macos"))]
    println!("  GPU backend:   check llama.cpp build flags");

    if db_path.exists() {
        let s = store::open_store_no_backend(index_dir, true)?;
        let cols = db::list_collections(&s.db)?;
        println!("\n  Collections:   {}", cols.len());
        for col in &cols {
            let (count, _) = db::collection_doc_stats(&s.db, &col.name)?;
            println!("    {} — {count} docs at {}", col.name, col.path);
        }

        print_doctor_stale_fingerprint_check(&s.db);
        print_doctor_orphaned_vector_check(&s.db);
        let (exclusions, excluded) = load_exclusions(&s.db);
        print_doctor_embed_exclusions(&s.db, &exclusions, &excluded);
        store::warn_if_raw_backfill_pending(&s);
        store::warn_if_fts_rebuild_pending(&s);

        // Recommended next steps.
        let needs_embed: i64 = db::count_docs_needing_embed_excluding(
            &s.db,
            &store::expected_fingerprint(),
            &store::expected_ast_fingerprints(),
            &excluded.ids,
        )
        .unwrap_or(0);
        if needs_embed > 0 {
            println!("\n  Recommended next step");
            println!("    Run 'rqmd embed' to generate embeddings ({needs_embed} hashes pending)");
        }
    }
    Ok(())
}

/// Stale-embedding check for `rqmd doctor`: an interrupted or not-yet-run
/// incremental `rqmd embed` can leave `content_vectors` rows under more than
/// the expected fingerprints after an embed-model or chunking-strategy change —
/// `rqmd embed` supersedes a hash's stale rows as it re-embeds each one, but
/// until that finishes, stale fingerprints coexist with current ones. The base
/// fingerprint and each language's AST one are *all* current — a path-scoped
/// marker means eligible and non-eligible paths legitimately embed under
/// different fingerprints — so the gate checks membership in that set, not
/// equality with a single value.
fn print_doctor_stale_fingerprint_check(conn: &Connection) {
    let breakdown = db::fingerprint_breakdown(conn).unwrap_or_default();
    let current = store::expected_fingerprint_set();
    if breakdown.iter().any(|(fp, _)| !current.contains(fp)) {
        println!(
            "\n  {} — vectors were generated by a different model/chunking config than the one active now:",
            fmt::Paint::stdout().yellow("⚠ Stale embedding fingerprint(s) detected")
        );
        for (fp, count) in &breakdown {
            let marker = if current.contains(fp) {
                " (current)"
            } else {
                " (stale)"
            };
            println!("    {fp}{marker} — {count} chunks");
        }
        println!("    Run 'rqmd embed --rebuild' to re-embed everything under the current model");
    }
}

/// Orphaned-vector check for `rqmd doctor`: `rqmd update` soft-deletes
/// documents whose file was removed or renamed (active=0) rather than
/// hard-deleting, so their content_vectors/HNSW vids survive but become
/// unreachable — every vector→document join requires an active document. Say
/// this plainly rather than implying the space is freed; `embed --cleanup`
/// reclaims it without a full re-embed.
fn print_doctor_orphaned_vector_check(conn: &Connection) {
    let orphaned_vectors = db::count_orphaned_vectors(conn).unwrap_or(0);
    if orphaned_vectors > 0 {
        println!(
            "\n  {} — left behind by documents removed or renamed since their last embed. Unreachable in search but not yet freed from the HNSW file.",
            fmt::Paint::stdout().yellow(format!("⚠ {orphaned_vectors} orphaned vector(s)"))
        );
        println!("    Run 'rqmd embed --cleanup' to reclaim the space");
    }
}

/// Embed-exclusion report for `rqmd doctor` (#147), printed only when some
/// collection has a rule or a cap: how many documents are left out and why,
/// and how many vectors a plain `rqmd embed` kept for them.
fn print_doctor_embed_exclusions(
    conn: &Connection,
    exclusions: &EmbedExclusions,
    excluded: &ExcludedDocs,
) {
    if exclusions.is_empty() {
        return;
    }
    println!("\n  Embed exclusions: {}", describe_excluded(excluded));
    let kept = db::count_excluded_only_vectors(conn, &excluded.ids).unwrap_or(0);
    if kept > 0 {
        println!(
            "    {kept} vector(s) kept for excluded documents; run 'rqmd embed --cleanup' to drop them"
        );
    }
}

/// Extensions of prose that the markdown chunker handles by design, which
/// `rqmd status` does not report as lacking a grammar.
const PROSE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "txt"];

/// The most extensions `rqmd status` names before summarising the rest.
const MAX_UNCOVERED_EXTENSIONS: usize = 8;

/// `.ext (n)` for the indexed extensions in `counts` (most common first) that
/// have no grammar and are not prose, empty when there are none.
fn extensions_without_grammar(counts: &[(String, i64)], grammars: &[&str]) -> String {
    let uncovered: Vec<String> = counts
        .iter()
        .filter(|(ext, _)| {
            !grammars.contains(&ext.as_str()) && !PROSE_EXTENSIONS.contains(&ext.as_str())
        })
        .map(|(ext, n)| {
            // File names are untrusted text: keep control characters out of the line.
            let ext: String = ext
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            format!(".{ext} ({n})")
        })
        .collect();
    match uncovered.len().checked_sub(MAX_UNCOVERED_EXTENSIONS) {
        Some(rest) if rest > 0 => format!(
            "{}, +{rest} more",
            uncovered[..MAX_UNCOVERED_EXTENSIONS].join(", ")
        ),
        _ => uncovered.join(", "),
    }
}

fn fmt_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b} B")
    } else if b < 1024 * 1024 {
        format!("{:.1} KB", b as f64 / 1024.0)
    } else if b < 1024 * 1024 * 1024 {
        format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn dir_size(dir: &Path) -> u64 {
    if !dir.exists() {
        return 0;
    }
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Truncate a collection-context string for the status display's one-line
/// preview. `ctx.len() > 60` is a *byte* length check, not a char count, so
/// the fixed cut point is snapped to a valid char boundary before slicing —
/// otherwise multi-byte UTF-8 straddling byte 57 panics.
fn truncate_context_preview(ctx: &str) -> String {
    if ctx.len() > 60 {
        let cut = snap_char_boundary_backward(ctx, 57);
        format!("{}...", &ctx[..cut])
    } else {
        ctx.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_without_grammar_skips_covered_and_prose_and_caps_the_list() {
        let counts: Vec<(String, i64)> = [("md", 9), ("go", 8), ("rb", 7), ("txt", 6)]
            .into_iter()
            .chain((0..9).map(|_| ("x", 1)))
            .map(|(e, n)| (e.to_string(), n))
            .collect();
        assert_eq!(extensions_without_grammar(&counts[..4], &["go"]), ".rb (7)");
        assert_eq!(extensions_without_grammar(&counts[..2], &["go"]), "");
        let capped = extensions_without_grammar(&counts, &["go"]);
        assert!(capped.starts_with(".rb (7), .x (1)"), "{capped}");
        assert!(capped.ends_with(", +2 more"), "{capped}");
    }

    #[test]
    fn extensions_without_grammar_strips_control_characters() {
        let hostile = vec![("a\x1b[2Jb".to_string(), 1)];
        let line = extensions_without_grammar(&hostile, &[]);
        assert_eq!(line, ".a?[2Jb (1)");
        assert!(!line.chars().any(char::is_control));
    }

    const TEST_HOOKS: HookConfig = HookConfig {
        timeout: std::time::Duration::from_secs(30),
        own_process_group: true,
    };

    const TEST_RUN: UpdateRun = UpdateRun {
        progress_mode: fmt::ProgressMode::Off,
        hooks_allowed: true,
        full: false,
        hook_config: TEST_HOOKS,
    };

    #[test]
    fn truncate_context_preview_snaps_multibyte_boundary() {
        // 56 ASCII bytes followed by a 2-byte 'é' straddling the fixed
        // byte-57 cut point. Total length (61) exceeds the 60-byte
        // threshold, but the naive `&ctx[..57]` would slice mid-character
        // (byte 57 is the second byte of 'é') and panic.
        let ctx = format!("{}éxyz", "a".repeat(56));
        assert_eq!(ctx.len(), 61);
        let preview = truncate_context_preview(&ctx);
        assert_eq!(preview, format!("{}...", "a".repeat(56)));
    }

    #[test]
    fn truncate_context_preview_leaves_short_context_untouched() {
        let ctx = "short context";
        assert_eq!(truncate_context_preview(ctx), ctx);
    }

    /// End-to-end regression for `run_update`'s global orphan-vector sweep
    /// (not just the `rqmd-core` helper it calls) — drives the real walk +
    /// prune path, then the same `reclaim_orphaned_vectors` call `run_update`
    /// makes once after every collection's deactivation pass has landed, so a
    /// wiring bug (wrong variable, skipped transaction, wrong flush ordering)
    /// would be caught here even though `rqmd-core`'s own integration test
    /// only exercises the helper directly.
    #[test]
    fn update_one_collection_evicts_orphaned_vectors_but_keeps_shared_hash() {
        let index_dir = tempfile::tempdir().unwrap();
        let keep_src = tempfile::tempdir().unwrap();
        let coll_src = tempfile::tempdir().unwrap();

        let shared_body = "# Shared\nshared orphan-check content";
        let unique_body = "# Unique\nunique orphan-check content";
        std::fs::write(keep_src.path().join("shared.md"), shared_body).unwrap();
        std::fs::write(coll_src.path().join("shared.md"), shared_body).unwrap();
        std::fs::write(coll_src.path().join("unique.md"), unique_body).unwrap();

        let mut s = store::open_store_no_backend(index_dir.path(), false).unwrap();

        let keep_col = Collection {
            name: "keep".into(),
            path: keep_src.path().to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
            embed_ignore: vec![],
            embed_max_bytes: None,
        };
        let coll_col = Collection {
            name: "coll".into(),
            path: coll_src.path().to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
            embed_ignore: vec![],
            embed_max_bytes: None,
        };
        db::upsert_collection(&s.db, &keep_col).unwrap();
        db::upsert_collection(&s.db, &coll_col).unwrap();

        // Initial walk indexes both collections' files as active documents.
        update_one_collection(&mut s, &keep_col, TEST_RUN, &|| {}, &|| None).unwrap();
        update_one_collection(&mut s, &coll_col, TEST_RUN, &|| {}, &|| None).unwrap();

        let shared_hash =
            db::hashes_for_paths(&s.db, "keep", &["shared.md".to_string()]).unwrap()[0].clone();
        let unique_hash =
            db::hashes_for_paths(&s.db, "coll", &["unique.md".to_string()]).unwrap()[0].clone();
        assert_eq!(
            db::hashes_for_paths(&s.db, "coll", &["shared.md".to_string()]).unwrap()[0],
            shared_hash,
            "both collections' shared.md must hash-dedupe to the same content row"
        );

        // Seed vectors for both hashes, as `rqmd embed` would have.
        let now = "2024-01-01T00:00:00Z";
        db::upsert_vector_meta(&s.db, &shared_hash, 0, 0, "fake", "fp", 1, 100, now).unwrap();
        db::upsert_vector_meta(&s.db, &unique_hash, 0, 0, "fake", "fp", 1, 101, now).unwrap();

        // Delete only "coll"'s unique.md and re-run its update — shared.md
        // stays on disk (so the mask still matches ≥1 file and the prune
        // guard against a 0-file mask/mount glitch doesn't short-circuit),
        // and "keep" still references shared_hash, so only unique_hash
        // should be reclaimed.
        std::fs::remove_file(coll_src.path().join("unique.md")).unwrap();
        update_one_collection(&mut s, &coll_col, TEST_RUN, &|| {}, &|| None).unwrap();
        s.reclaim_orphaned_vectors().unwrap();

        assert!(
            db::hash_has_any_vector(&s.db, &shared_hash),
            "shared_hash's vector must survive — 'keep' still actively references it"
        );
        assert!(
            !db::hash_has_any_vector(&s.db, &unique_hash),
            "unique_hash's vector must be reclaimed — no active document references it anymore"
        );
    }

    /// A signal that lands while a file is being indexed lets that file finish,
    /// stops the walk before the next one, and prunes nothing: the files that
    /// are gone from disk are left for the rerun.
    #[cfg(unix)]
    #[test]
    fn a_signal_stops_the_walk_before_the_next_file_and_prunes_nothing() {
        let _catch = signals::Catch::install_exiting_on_repeat();
        let index_dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("gone.md"), "soon deleted").unwrap();
        let col = test_collection("c", src.path());
        let mut s = store::open_store_no_backend(index_dir.path(), false).unwrap();
        db::upsert_collection(&s.db, &col).unwrap();
        update_one_collection(&mut s, &col, TEST_RUN, &|| {}, &|| None).unwrap();

        std::fs::remove_file(src.path().join("gone.md")).unwrap();
        for i in 0..5 {
            std::fs::write(src.path().join(format!("note{i}.md")), format!("body {i}")).unwrap();
        }
        let files_seen = std::cell::Cell::new(0);
        let heartbeat = || {
            files_seen.set(files_seen.get() + 1);
            if files_seen.get() == 3 {
                // SAFETY: plain syscall; the handler under test only records it.
                unsafe { libc::raise(libc::SIGTERM) };
            }
        };

        let outcome =
            update_one_collection(&mut s, &col, TEST_RUN, &heartbeat, &signals::pending).unwrap();

        assert_eq!(outcome.interrupted, Some(libc::SIGTERM));
        assert!(!outcome.clean);
        let paths: HashSet<String> = db::list_documents(&s.db, Some("c"))
            .unwrap()
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(
            paths.len(),
            3,
            "two notes before the signal, and gone.md: {paths:?}"
        );
        assert!(
            paths.contains("gone.md"),
            "nothing is pruned after a signal"
        );
    }

    /// `run_cleanup` end-to-end: a shared hash referenced by an active
    /// document must survive the sweep even though its sibling document was
    /// removed, and a second cleanup pass must be a no-op (idempotent).
    #[test]
    fn run_cleanup_sweeps_orphans_keeps_shared_hash_and_is_idempotent() {
        let index_dir = tempfile::tempdir().unwrap();

        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            let now = "2026-01-01T00:00:00Z";
            db::upsert_content(&s.db, "shared_hash", "shared body", now).unwrap();
            db::upsert_document(&s.db, "col", "a.md", "A", "shared_hash", now).unwrap();
            db::upsert_document(&s.db, "col", "b.md", "B", "shared_hash", now).unwrap();
            db::upsert_content(&s.db, "orphan_hash", "orphan body", now).unwrap();
            db::upsert_document(&s.db, "col", "c.md", "C", "orphan_hash", now).unwrap();
            db::upsert_vector_meta(&s.db, "shared_hash", 0, 0, "m", "fp", 1, 900, now).unwrap();
            db::upsert_vector_meta(&s.db, "orphan_hash", 0, 0, "m", "fp", 1, 901, now).unwrap();
            // Soft-delete c.md so orphan_hash is orphaned but still row-referenced.
            conn_deactivate(&s.db, "c.md");
        }

        run_cleanup(index_dir.path()).unwrap();

        {
            let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
            assert_eq!(db::count_orphaned_vectors(&s.db).unwrap(), 0);
            assert!(
                db::hash_has_any_vector(&s.db, "shared_hash"),
                "shared_hash is still actively referenced and must keep its vector"
            );
            assert!(!db::hash_has_any_vector(&s.db, "orphan_hash"));
        }

        // Second pass is a no-op: nothing left to sweep.
        run_cleanup(index_dir.path()).unwrap();
        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert_eq!(db::count_orphaned_vectors(&s.db).unwrap(), 0);
    }

    /// `clear_vectors_for_collection` + `rebuildable_vids_for_collection` must
    /// agree with each other and must not touch a hash shared with another
    /// active collection — the SQL-layer half of #66. `a`'s own vector is
    /// cleared, `b`'s vector survives, and the shared hash's vector survives
    /// because `b` still actively references it.
    #[test]
    fn scoped_rebuild_clear_keeps_hash_shared_with_another_collection() {
        let index_dir = tempfile::tempdir().unwrap();
        let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
        let now = "2026-01-01T00:00:00Z";

        db::upsert_content(&s.db, "a_only_hash", "a only body", now).unwrap();
        db::upsert_document(&s.db, "a", "a_only.md", "A", "a_only_hash", now).unwrap();
        db::upsert_vector_meta(&s.db, "a_only_hash", 0, 0, "m", "fp", 1, 1, now).unwrap();

        db::upsert_content(&s.db, "b_only_hash", "b only body", now).unwrap();
        db::upsert_document(&s.db, "b", "b_only.md", "B", "b_only_hash", now).unwrap();
        db::upsert_vector_meta(&s.db, "b_only_hash", 0, 0, "m", "fp", 1, 2, now).unwrap();

        db::upsert_content(&s.db, "shared_hash", "shared body", now).unwrap();
        db::upsert_document(&s.db, "a", "shared.md", "S", "shared_hash", now).unwrap();
        db::upsert_document(&s.db, "b", "shared.md", "S", "shared_hash", now).unwrap();
        db::upsert_vector_meta(&s.db, "shared_hash", 0, 0, "m", "fp", 1, 3, now).unwrap();

        let vids = db::rebuildable_vids_for_collection(&s.db, "a").unwrap();
        assert_eq!(
            vids,
            vec![1],
            "only a_only_hash's vid is rebuildable for 'a' — shared_hash is excluded"
        );

        db::clear_vectors_for_collection(&s.db, "a").unwrap();

        assert!(
            !db::hash_has_any_vector(&s.db, "a_only_hash"),
            "a's own vector must be cleared by a scoped rebuild"
        );
        assert!(
            db::hash_has_any_vector(&s.db, "b_only_hash"),
            "b's own vector must survive a rebuild scoped to 'a'"
        );
        assert!(
            db::hash_has_any_vector(&s.db, "shared_hash"),
            "shared_hash's vector must survive — 'b' still actively references it"
        );
    }

    /// Content referenced by no document at all is deleted; content still
    /// referenced by a soft-deleted (inactive) document survives — cleanup
    /// must not touch anything a document row still points to.
    #[test]
    fn run_cleanup_deletes_unreferenced_content_but_keeps_inactive_referenced_content() {
        let index_dir = tempfile::tempdir().unwrap();
        let now = "2026-01-01T00:00:00Z";

        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::upsert_content(&s.db, "kept_hash", "kept body", now).unwrap();
            db::upsert_document(&s.db, "col", "kept.md", "K", "kept_hash", now).unwrap();
            conn_deactivate(&s.db, "kept.md");
            db::upsert_content(&s.db, "gone_hash", "gone body", now).unwrap();
        }

        run_cleanup(index_dir.path()).unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        let count: i64 =
            s.db.query_row("SELECT COUNT(*) FROM content", [], |r| r.get(0))
                .unwrap();
        assert_eq!(
            count, 1,
            "only the fully-unreferenced hash should be deleted"
        );
        let remaining_hash: String =
            s.db.query_row("SELECT hash FROM content", [], |r| r.get(0))
                .unwrap();
        assert_eq!(remaining_hash, "kept_hash");
    }

    fn conn_deactivate(conn: &Connection, path: &str) {
        conn.execute("UPDATE documents SET active = 0 WHERE path = ?1", [path])
            .unwrap();
    }

    struct TestBackend {
        model: &'static str,
        /// Embedding any text containing this marker fails.
        fail_on: Option<&'static str>,
        delay: Duration,
        /// Raise this signal on this process during the n-th embed call (1-based).
        signal_on_call: Option<(usize, i32)>,
        calls: usize,
    }

    impl rqmd_llm::InferenceBackend for TestBackend {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
            std::thread::sleep(self.delay);
            self.calls += 1;
            if let Some((call, signal)) = self.signal_on_call
                && call == self.calls
            {
                // SAFETY: plain syscall; the handler under test only records it.
                unsafe { libc::raise(signal) };
            }
            if self.fail_on.is_some_and(|marker| text.contains(marker)) {
                anyhow::bail!("injected embed failure");
            }
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
            unreachable!()
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            unreachable!()
        }
        fn embed_model_name(&self) -> &str {
            self.model
        }
        fn rerank_model_name(&self) -> &str {
            "test-rerank"
        }
        fn generate_model_name(&self) -> &str {
            "test-generate"
        }
    }

    const POISON: &str = "POISONED-BODY";

    fn embed_store(
        dir: &Path,
        model: &'static str,
        fail_on: Option<&'static str>,
    ) -> rqmd_core::Store {
        rqmd_core::Store::open(
            store::store_config(dir, false),
            Box::new(TestBackend {
                model,
                fail_on,
                delay: Duration::ZERO,
                signal_on_call: None,
                calls: 0,
            }),
        )
        .unwrap()
    }

    fn test_collection(name: &str, path: &Path) -> Collection {
        Collection {
            name: name.into(),
            path: path.to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
            respect_gitignore: false,
            max_file_bytes: None,
            embed_ignore: vec![],
            embed_max_bytes: None,
        }
    }

    /// Index `bodies` as documents `doc0.md`, `doc1.md`, … of collection `c`.
    fn seed_docs(s: &mut rqmd_core::Store, bodies: &[&str]) -> Vec<Document> {
        db::upsert_collection(&s.db, &test_collection("c", Path::new("/unused"))).unwrap();
        for (i, body) in bodies.iter().enumerate() {
            let path = format!("doc{i}.md");
            s.index_document_fts_only_with_raw("c", &path, &path, body, body)
                .unwrap();
        }
        let mut docs = db::list_documents(&s.db, Some("c")).unwrap();
        docs.sort_by(|a, b| a.path.cmp(&b.path));
        docs
    }

    fn run_embed_docs(
        s: &mut rqmd_core::Store,
        docs: &[Document],
        interval: usize,
    ) -> (EmbedRun, EmbedFlow) {
        run_embed_docs_until(s, docs, interval, &|| None)
    }

    fn run_embed_docs_until(
        s: &mut rqmd_core::Store,
        docs: &[Document],
        interval: usize,
        interrupted: &dyn Fn() -> Option<i32>,
    ) -> (EmbedRun, EmbedFlow) {
        let mut run = EmbedRun::new(interval);
        let refs: Vec<&Document> = docs.iter().collect();
        let flow = embed_docs(
            s,
            &mut run,
            &refs,
            Deadline::none(),
            interrupted,
            |_, _, _| {},
        )
        .unwrap();
        checkpoint(s, &mut run.pending, &mut run.pending_deletes).unwrap();
        (run, flow)
    }

    fn vector_models(s: &rqmd_core::Store, hash: &str) -> Vec<String> {
        let mut stmt =
            s.db.prepare("SELECT model FROM content_vectors WHERE hash = ?1")
                .unwrap();
        stmt.query_map([hash], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_failing_document_is_skipped_and_the_rest_are_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = embed_store(dir.path(), "m", Some(POISON));
        let docs = seed_docs(&mut s, &["first body", POISON, "third body"]);

        let (run, flow) = run_embed_docs(&mut s, &docs, 50);

        assert_eq!(flow, EmbedFlow::Continue);
        assert_eq!(run.new_docs, 2);
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].path, "doc1.md");
        assert!(run.failures[0].error.contains("injected embed failure"));
        assert!(db::hash_has_any_vector(&s.db, &docs[0].hash));
        assert!(!db::hash_has_any_vector(&s.db, &docs[1].hash));
        assert!(db::hash_has_any_vector(&s.db, &docs[2].hash));
    }

    #[test]
    fn a_failed_document_keeps_its_existing_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let docs = {
            let mut old = embed_store(dir.path(), "old-model", None);
            let docs = seed_docs(&mut old, &["first body", POISON, "third body"]);
            run_embed_docs(&mut old, &docs, 50);
            docs
        };

        let mut s = embed_store(dir.path(), "new-model", Some(POISON));
        let before = db::vids_for_hash(&s.db, &docs[1].hash).unwrap();
        assert_eq!(before.len(), 1);
        let (run, _) = run_embed_docs(&mut s, &docs, 50);

        assert_eq!(run.failures.len(), 1);
        assert_eq!(
            db::vids_for_hash(&s.db, &docs[1].hash).unwrap(),
            before,
            "the failed document's old vector rows must survive"
        );
        assert_eq!(vector_models(&s, &docs[1].hash), vec!["old-model"]);
        assert_eq!(vector_models(&s, &docs[0].hash), vec!["new-model"]);
        assert_eq!(vector_models(&s, &docs[2].hash), vec!["new-model"]);
        assert_eq!(
            s.hnsw_size(),
            3,
            "the failed document's old vector must still be in the index"
        );
    }

    #[test]
    fn a_stale_document_that_triggers_a_checkpoint_keeps_its_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let docs = {
            let mut old = embed_store(dir.path(), "old-model", None);
            let docs = seed_docs(&mut old, &["fresh body", "stale body"]);
            run_embed_docs(&mut old, &docs[1..], 50);
            docs
        };

        let mut s = embed_store(dir.path(), "new-model", None);
        // Interval 2: the stale second document's own success fires a checkpoint.
        let (run, flow) = run_embed_docs(&mut s, &docs, 2);

        assert_eq!(flow, EmbedFlow::Continue);
        assert_eq!(run.new_docs, 2);
        assert_eq!(vector_models(&s, &docs[1].hash), vec!["new-model"]);
        assert_eq!(vector_models(&s, &docs[0].hash), vec!["new-model"]);
    }

    #[test]
    fn embedding_stops_at_the_deadline_and_keeps_what_was_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = rqmd_core::Store::open(
            store::store_config(dir.path(), false),
            Box::new(TestBackend {
                model: "m",
                fail_on: None,
                delay: Duration::from_millis(200),
                signal_on_call: None,
                calls: 0,
            }),
        )
        .unwrap();
        let docs = seed_docs(&mut s, &["first body", "second body", "third body"]);
        let mut run = EmbedRun::new(50);
        let refs: Vec<&Document> = docs.iter().collect();

        let flow = embed_docs(
            &mut s,
            &mut run,
            &refs,
            Deadline::after(Duration::from_millis(100)),
            &|| None,
            |_, _, _| {},
        )
        .unwrap();
        checkpoint(&mut s, &mut run.pending, &mut run.pending_deletes).unwrap();

        assert!(matches!(flow, EmbedFlow::TimedOut(t) if t.stage == "the next document"));
        assert_eq!(run.new_docs, 1);
        assert!(db::hash_has_any_vector(&s.db, &docs[0].hash));
        assert!(!db::hash_has_any_vector(&s.db, &docs[1].hash));

        let end = finish_embed(&run, &flow).unwrap_err();
        assert!(end.downcast_ref::<TimedOut>().is_some());
    }

    fn signalling_store(dir: &Path, signal_on_call: (usize, i32)) -> rqmd_core::Store {
        rqmd_core::Store::open(
            store::store_config(dir, false),
            Box::new(TestBackend {
                model: "m",
                fail_on: None,
                delay: Duration::ZERO,
                signal_on_call: Some(signal_on_call),
                calls: 0,
            }),
        )
        .unwrap()
    }

    /// A signal that lands while a document is embedding lets that document
    /// finish and checkpoint, then stops before the next one.
    #[cfg(unix)]
    #[test]
    fn a_signal_during_a_document_lets_it_finish_then_stops_with_its_work_saved() {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let _catch = signals::Catch::install_exiting_on_repeat();
            let dir = tempfile::tempdir().unwrap();
            let mut s = signalling_store(dir.path(), (2, signal));
            let docs = seed_docs(&mut s, &["one", "two", "three", "four", "five"]);

            let (run, flow) = run_embed_docs_until(&mut s, &docs, 50, &signals::pending);

            assert_eq!(flow, EmbedFlow::Interrupted(signal));
            assert_eq!(run.new_docs, 2, "the document being embedded finishes");
            assert!(db::hash_has_any_vector(&s.db, &docs[0].hash));
            assert!(db::hash_has_any_vector(&s.db, &docs[1].hash));
            assert!(!db::hash_has_any_vector(&s.db, &docs[2].hash));

            let end = finish_embed(&run, &flow).unwrap_err();
            assert_eq!(end.downcast_ref::<Interrupted>().map(|i| i.0), Some(signal));
        }
    }

    #[test]
    fn consecutive_failures_abort_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = embed_store(dir.path(), "m", Some(POISON));
        let bodies: Vec<String> = (0..8).map(|i| format!("{POISON} {i}")).collect();
        let docs = seed_docs(
            &mut s,
            &bodies.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let (run, flow) = run_embed_docs(&mut s, &docs, 50);

        assert_eq!(flow, EmbedFlow::Abort);
        assert_eq!(run.failures.len(), MAX_CONSECUTIVE_EMBED_FAILURES);
    }

    #[test]
    fn a_success_resets_the_consecutive_failure_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = embed_store(dir.path(), "m", Some(POISON));
        let mut bodies: Vec<String> = Vec::new();
        for round in 0..2 {
            bodies.extend((0..4).map(|i| format!("{POISON} {round}-{i}")));
            bodies.push(format!("good body {round}"));
        }
        let docs = seed_docs(
            &mut s,
            &bodies.iter().map(String::as_str).collect::<Vec<_>>(),
        );

        let (run, flow) = run_embed_docs(&mut s, &docs, 50);

        assert_eq!(flow, EmbedFlow::Continue);
        assert_eq!(run.failures.len(), 8);
        assert_eq!(run.new_docs, 2);
    }

    #[test]
    fn finish_embed_maps_failures_to_partial_failure_or_a_hard_error() {
        let mut run = EmbedRun::new(50);
        assert!(finish_embed(&run, &EmbedFlow::Continue).is_ok());

        run.failures.push(EmbedFailure {
            collection: "c".into(),
            path: "a.md".into(),
            error: "boom".into(),
        });
        let partial = finish_embed(&run, &EmbedFlow::Continue).unwrap_err();
        assert!(partial.downcast_ref::<PartialFailure>().is_some());

        let abort = finish_embed(&run, &EmbedFlow::Abort).unwrap_err();
        assert!(abort.downcast_ref::<PartialFailure>().is_none());
        assert!(abort.to_string().contains("consecutive"));
    }

    fn hook_collection(name: &str, src: &Path, hook: Option<&str>) -> Collection {
        let mut col = test_collection(name, src);
        col.update_command = hook.map(str::to_string);
        col
    }

    /// A hung hook and a failing hook must not stop later collections from being
    /// indexed, and the run must still end as a `PartialFailure`.
    #[test]
    fn run_update_survives_hook_timeout_and_failure_and_reports_them() {
        let index_dir = tempfile::tempdir().unwrap();
        let srcs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        for (i, src) in srcs.iter().enumerate() {
            std::fs::write(src.path().join("note.md"), format!("# note {i}\nbody {i}")).unwrap();
        }
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            let hooks = [Some("sleep 20"), Some("exit 3"), None];
            for (i, (src, hook)) in srcs.iter().zip(hooks).enumerate() {
                db::upsert_collection(&s.db, &hook_collection(&format!("c{i}"), src.path(), hook))
                    .unwrap();
            }
        }
        let config = HookConfig {
            timeout: std::time::Duration::from_millis(300),
            own_process_group: true,
        };

        let err = run_update_with(index_dir.path(), None, true, false, config).unwrap_err();

        let partial = err
            .downcast_ref::<PartialFailure>()
            .expect("PartialFailure");
        assert!(partial.0.contains("2 update hook"), "{partial}");
        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        for name in ["c0", "c1", "c2"] {
            assert_eq!(
                db::list_documents(&s.db, Some(name)).unwrap().len(),
                1,
                "{name} must be indexed despite the earlier hooks failing"
            );
        }
    }

    #[test]
    fn run_update_rebuilds_a_legacy_full_text_index() {
        let index_dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("jp.md"),
            "# 旅行記\n\n私は昨日東京都庁を訪れました",
        )
        .unwrap();
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::upsert_collection(&s.db, &test_collection("c", src.path())).unwrap();
        }
        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();

        // Make the index look like one built by the previous tokenizer: no
        // version marker, and the document absent from Tantivy. The file is
        // unchanged, so only the rebuild can bring it back.
        {
            let mut s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::delete_config(&s.db, rqmd_core::store::FTS_TOKENIZER_CONFIG_KEY).unwrap();
            s.remove_from_fts("c/jp.md").unwrap();
            s.flush().unwrap();
            assert!(s.search_fts("東京都", 10, None).unwrap().is_empty());
            assert!(s.fts_rebuild_pending().unwrap());
        }

        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert_eq!(s.search_fts("東京都", 10, None).unwrap().len(), 1);
        assert!(!s.fts_rebuild_pending().unwrap());
    }

    /// An index with a Japanese document in collection `c` and a second
    /// collection `gone` whose directory no longer exists, so every unscoped
    /// update is "unclean". Returns the index dir and the live source dir, which
    /// the caller keeps alive for the test's duration.
    fn index_with_a_missing_collection_dir() -> (tempfile::TempDir, tempfile::TempDir) {
        let index_dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("jp.md"),
            "# 旅行記\n\n私は昨日東京都庁を訪れました",
        )
        .unwrap();
        let missing = tempfile::tempdir().unwrap();
        let missing_path = missing.path().to_path_buf();
        drop(missing);
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::upsert_collection(&s.db, &test_collection("c", src.path())).unwrap();
            db::upsert_collection(&s.db, &test_collection("gone", &missing_path)).unwrap();
        }
        (index_dir, src)
    }

    /// Make the index look built by the previous tokenizer: no version marker
    /// and the Japanese document absent from Tantivy.
    fn make_full_text_index_legacy(index_dir: &Path) {
        let mut s = store::open_store_no_backend(index_dir, false).unwrap();
        db::delete_config(&s.db, rqmd_core::store::FTS_TOKENIZER_CONFIG_KEY).unwrap();
        s.remove_from_fts("c/jp.md").unwrap();
        s.flush().unwrap();
        assert!(s.fts_rebuild_pending().unwrap());
    }

    #[test]
    fn run_update_rebuilds_the_full_text_index_despite_a_missing_collection_dir() {
        let (index_dir, _src) = index_with_a_missing_collection_dir();
        // The first update is unclean (missing dir), so it never certifies the
        // raw-split backfill; simulate an index that an earlier clean pass did.
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            store::mark_raw_split_backfilled(&s).unwrap();
        }
        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();
        make_full_text_index_legacy(index_dir.path());

        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert_eq!(s.search_fts("東京都", 10, None).unwrap().len(), 1);
        assert!(!s.fts_rebuild_pending().unwrap());
    }

    #[test]
    fn a_legacy_index_with_an_unclean_pass_still_defers_the_rebuild() {
        let (index_dir, _src) = index_with_a_missing_collection_dir();
        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();
        make_full_text_index_legacy(index_dir.path());

        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert!(!store::raw_split_backfilled(&s));
        assert!(s.fts_rebuild_pending().unwrap());
    }

    #[test]
    fn a_collection_scoped_update_leaves_the_full_text_rebuild_pending() {
        let index_dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("jp.md"),
            "# 旅行記\n\n私は昨日東京都庁を訪れました",
        )
        .unwrap();
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::upsert_collection(&s.db, &test_collection("c", src.path())).unwrap();
        }

        run_update_with(index_dir.path(), Some("c"), true, false, TEST_HOOKS).unwrap();

        let s = store::open_store_no_backend(index_dir.path(), true).unwrap();
        assert!(s.fts_rebuild_pending().unwrap());
    }

    #[test]
    fn run_update_succeeds_when_every_hook_succeeds() {
        let index_dir = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("note.md"), "# note\nbody").unwrap();
        {
            let s = store::open_store_no_backend(index_dir.path(), false).unwrap();
            db::upsert_collection(&s.db, &hook_collection("c", src.path(), Some("exit 0")))
                .unwrap();
        }
        run_update_with(index_dir.path(), None, true, false, TEST_HOOKS).unwrap();
    }

    // ── Chunk-vector reuse across `update` → `embed` ─────────────────────────

    type EncodedLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// Embeds deterministically from the text and records every text it is
    /// asked to encode, under the model name the CLI itself would fingerprint.
    struct CountingBackend {
        model: String,
        encoded: EncodedLog,
    }

    impl rqmd_llm::InferenceBackend for CountingBackend {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
            use std::hash::{Hash, Hasher};
            self.encoded.lock().unwrap().push(text.to_string());
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            let seed = h.finish();
            Ok((0..rqmd_llm::EMBED_DIM)
                .map(|i| ((seed >> (i % 56)) & 0xff) as f32 / 255.0 + 0.01)
                .collect())
        }
        fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
            unreachable!()
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            unreachable!()
        }
        fn embed_model_name(&self) -> &str {
            &self.model
        }
        fn rerank_model_name(&self) -> &str {
            "test-rerank"
        }
        fn generate_model_name(&self) -> &str {
            "test-generate"
        }
    }

    /// A note of `sections` sections, each long enough to fill its own chunk.
    /// `edit` swaps one sentence of the last section for different text of the
    /// same length, so every earlier chunk boundary stays where it was.
    fn sectioned_note(sections: usize, edit: &str) -> String {
        (0..sections)
            .map(|n| {
                let tail = if n + 1 == sections {
                    edit
                } else {
                    "the original closing remark written"
                };
                let filler: String = (0..40)
                    .map(|k| format!("Section {n} sentence {k} describes topic {n}-{k}. "))
                    .collect();
                format!("# Section {n}\n\n{filler}{tail}.\n\n")
            })
            .collect()
    }

    const ORIGINAL_TAIL: &str = "the original closing remark written";
    const EDITED_TAIL: &str = "an entirely different closing remark";
    const SECOND_EDIT_TAIL: &str = "yet another distinct closing remarks";

    struct EmbedFixture {
        index_dir: tempfile::TempDir,
        src: tempfile::TempDir,
        encoded: EncodedLog,
    }

    impl EmbedFixture {
        fn new(first_body: &str) -> Self {
            let fx = Self {
                index_dir: tempfile::tempdir().unwrap(),
                src: tempfile::tempdir().unwrap(),
                encoded: EncodedLog::default(),
            };
            std::fs::write(fx.src.path().join("note.md"), first_body).unwrap();
            {
                let s = store::open_store_no_backend(fx.index_dir.path(), false).unwrap();
                db::upsert_collection(&s.db, &test_collection("c", fx.src.path())).unwrap();
            }
            fx
        }

        fn write(&self, rel: &str, body: &str) {
            std::fs::write(self.src.path().join(rel), body).unwrap();
        }

        fn update(&self) {
            run_update_with(self.index_dir.path(), None, true, false, TEST_HOOKS).unwrap();
        }

        /// The embed pass `rqmd embed` runs, against the counting backend.
        fn embed(&self) {
            let _lock = IndexLock::acquire(self.index_dir.path()).unwrap();
            let backend = CountingBackend {
                model: rqmd_llm::BackendKind::from_env().default_embed_model_name(),
                encoded: self.encoded.clone(),
            };
            let s = rqmd_core::Store::open(
                store::store_config(self.index_dir.path(), false),
                Box::new(backend),
            )
            .unwrap();
            let cols = db::list_collections(&s.db).unwrap();
            embed_collections(s, &cols, false, None, &_lock).unwrap();
        }

        fn take_encoded(&self) -> Vec<String> {
            let mut got = std::mem::take(&mut *self.encoded.lock().unwrap());
            got.sort();
            got
        }

        fn store(&self) -> rqmd_core::Store {
            store::open_store_no_backend(self.index_dir.path(), true).unwrap()
        }
    }

    fn chunk_texts(path: &str, body: &str) -> HashSet<String> {
        rqmd_core::chunking::chunk_document_for_path(path, body)
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    fn changed_chunks(path: &str, before: &str, after: &str) -> Vec<String> {
        let old = chunk_texts(path, before);
        let mut changed: Vec<String> = chunk_texts(path, after)
            .into_iter()
            .filter(|t| !old.contains(t))
            .collect();
        changed.sort();
        changed
    }

    /// Chunk texts are long, so a mismatch reports counts rather than the texts.
    fn assert_encoded_exactly(got: &[String], want: &[String]) {
        assert!(
            got == want,
            "only the edited chunks may be encoded: encoded {} chunk(s), expected {}",
            got.len(),
            want.len()
        );
    }

    fn vector_rows(s: &rqmd_core::Store) -> i64 {
        s.db.query_row("SELECT COUNT(*) FROM content_vectors", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn an_edit_after_update_encodes_only_the_changed_chunks() {
        let before = sectioned_note(5, ORIGINAL_TAIL);
        let after = sectioned_note(5, EDITED_TAIL);
        let fx = EmbedFixture::new(&before);
        fx.update();
        fx.embed();
        let first = fx.take_encoded();
        assert!(first.len() >= 3, "fixture must span several chunks");

        fx.write("note.md", &after);
        fx.update();
        fx.embed();

        let changed = changed_chunks("note.md", &before, &after);
        assert!(!changed.is_empty() && changed.len() < first.len());
        assert_encoded_exactly(&fx.take_encoded(), &changed);
    }

    #[test]
    fn the_superseded_vectors_are_swept_once_the_edit_is_embedded() {
        let fx = EmbedFixture::new(&sectioned_note(4, ORIGINAL_TAIL));
        fx.update();
        fx.embed();
        let old_hash = db::list_documents(&fx.store().db, Some("c")).unwrap()[0]
            .hash
            .clone();

        fx.write("note.md", &sectioned_note(4, EDITED_TAIL));
        fx.update();
        fx.embed();

        let s = fx.store();
        assert!(!db::hash_has_any_vector(&s.db, &old_hash));
        assert_eq!(db::count_orphaned_vectors(&s.db).unwrap(), 0);
        let docs = db::list_documents(&s.db, Some("c")).unwrap();
        let rows = vector_rows(&s);
        assert_eq!(
            rows,
            db::vids_for_hash(&s.db, &docs[0].hash).unwrap().len() as i64,
            "only the edited document's own vectors remain"
        );
        assert_eq!(
            s.hnsw_size() as i64,
            rows,
            "no vector is left behind in the index"
        );
    }

    #[test]
    fn update_keeps_the_superseded_vectors_and_doctor_counts_no_orphans() {
        let fx = EmbedFixture::new(&sectioned_note(4, ORIGINAL_TAIL));
        fx.update();
        fx.embed();
        let rows_before = vector_rows(&fx.store());

        fx.write("note.md", &sectioned_note(4, EDITED_TAIL));
        fx.update();

        let s = fx.store();
        assert_eq!(
            vector_rows(&s),
            rows_before,
            "update must not sweep the donor vectors"
        );
        assert_eq!(db::count_orphaned_vectors(&s.db).unwrap(), 0);
        assert!(db::orphaned_vector_hashes(&s.db).unwrap().is_empty());
    }

    #[test]
    fn two_edits_before_an_embed_still_reuse_the_embedded_chunks() {
        let original = sectioned_note(5, ORIGINAL_TAIL);
        let second = sectioned_note(5, SECOND_EDIT_TAIL);
        let fx = EmbedFixture::new(&original);
        fx.update();
        fx.embed();
        fx.take_encoded();

        fx.write("note.md", &sectioned_note(5, EDITED_TAIL));
        fx.update();
        fx.write("note.md", &second);
        fx.update();
        fx.embed();

        assert_encoded_exactly(
            &fx.take_encoded(),
            &changed_chunks("note.md", &original, &second),
        );
        assert_eq!(db::count_orphaned_vectors(&fx.store().db).unwrap(), 0);
    }

    #[test]
    fn a_removed_document_is_still_swept_by_update() {
        let fx = EmbedFixture::new(&sectioned_note(3, ORIGINAL_TAIL));
        fx.write("other.md", "# Other\n\nanother small note body");
        fx.update();
        fx.embed();
        let removed_hash =
            db::hashes_for_paths(&fx.store().db, "c", &["note.md".to_string()]).unwrap()[0].clone();

        // A new, not yet embedded note sits beside the removal: it has no
        // vectors and no `prev_hash`, which must not hide the orphan.
        fx.write("fresh.md", "# Fresh\n\na note nothing has embedded yet");
        std::fs::remove_file(fx.src.path().join("note.md")).unwrap();
        fx.update();

        assert!(!db::hash_has_any_vector(&fx.store().db, &removed_hash));
    }

    #[test]
    fn an_edit_to_text_already_embedded_elsewhere_leaves_nothing_spared() {
        let shared = "# Shared\n\nshared body that another note already carries";
        let fx = EmbedFixture::new(&sectioned_note(3, ORIGINAL_TAIL));
        fx.write("twin.md", shared);
        fx.update();
        fx.embed();
        let old_hash =
            db::hashes_for_paths(&fx.store().db, "c", &["note.md".to_string()]).unwrap()[0].clone();

        fx.write("note.md", shared);
        fx.update();

        let s = fx.store();
        assert!(
            !db::hash_has_any_vector(&s.db, &old_hash),
            "the old hash has no pending successor to donate to"
        );
        let spared: i64 =
            s.db.query_row(
                "SELECT COUNT(*) FROM documents WHERE prev_hash IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spared, 0);
    }

    fn unkeyed_vector_rows(index_dir: &Path) -> i64 {
        rusqlite::Connection::open(index_dir.join("index.sqlite"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM content_vectors WHERE vector_key IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn identical_chunks_in_two_documents_are_encoded_once_in_one_run() {
        let a = sectioned_note(5, ORIGINAL_TAIL);
        let b = sectioned_note(5, EDITED_TAIL);
        let fx = EmbedFixture::new(&a);
        fx.write("other.md", &b);
        fx.update();
        fx.embed();

        let mut want: Vec<String> = chunk_texts("note.md", &a)
            .union(&chunk_texts("other.md", &b))
            .cloned()
            .collect();
        want.sort();
        assert!(want.len() >= 4, "fixture must span several chunks");
        assert_encoded_exactly(&fx.take_encoded(), &want);
    }

    #[test]
    fn embed_gives_vectors_from_the_previous_release_a_reuse_key() {
        let before = sectioned_note(5, ORIGINAL_TAIL);
        let after = sectioned_note(5, EDITED_TAIL);
        let fx = EmbedFixture::new(&before);
        fx.update();
        fx.embed();
        fx.take_encoded();
        rusqlite::Connection::open(fx.index_dir.path().join("index.sqlite"))
            .unwrap()
            .execute("UPDATE content_vectors SET vector_key = NULL", [])
            .unwrap();
        assert!(unkeyed_vector_rows(fx.index_dir.path()) > 0);

        fx.write("other.md", &after);
        fx.update();
        fx.embed();

        assert_eq!(unkeyed_vector_rows(fx.index_dir.path()), 0);
        assert_encoded_exactly(
            &fx.take_encoded(),
            &changed_chunks("other.md", &before, &after),
        );
    }

    // ── Embed exclusions (#147), against the counting backend ─────────────────

    /// Files whose bodies carry one unique marker word each. `fm.md` has a
    /// frontmatter block that makes the raw file larger than 200 bytes while
    /// its indexed text (the frontmatter stripped) stays under it.
    fn write_exclusion_fixture(fx: &EmbedFixture) {
        std::fs::create_dir_all(fx.src.path().join("gen")).unwrap();
        fx.write("keep.md", "# Keep\nGlacierword hiking routes.\n");
        fx.write("gen/skip.md", "# Skip\nZanzibarword generated listing.\n");
        fx.write(
            "fm.md",
            &format!(
                "---\ndescription: {}\n---\n# Small\nKiteword body.\n",
                "x".repeat(600)
            ),
        );
        fx.write(
            "big.md",
            &format!("# Big\nBarnacleword {}\n", "filler words ".repeat(40)),
        );
    }

    fn encoded_mentions(encoded: &[String], word: &str) -> bool {
        encoded.iter().any(|t| t.contains(word))
    }

    fn set_embed_rules(fx: &EmbedFixture, ignore: &[&str], cap: Option<u64>) {
        let s = store::open_store_no_backend(fx.index_dir.path(), false).unwrap();
        let ignore: Vec<String> = ignore.iter().map(|p| p.to_string()).collect();
        assert!(db::set_collection_embed_rules(&s.db, "c", &ignore, cap).unwrap());
    }

    fn hash_of(fx: &EmbedFixture, path: &str) -> String {
        db::list_documents(&fx.store().db, Some("c"))
            .unwrap()
            .into_iter()
            .find(|d| d.path == path)
            .unwrap()
            .hash
    }

    /// AC-1: with no rule and no cap every document is encoded, as before.
    #[test]
    fn embed_without_embed_rules_encodes_every_document() {
        let fx = EmbedFixture::new("# Note\nNoteword.\n");
        write_exclusion_fixture(&fx);
        fx.update();
        fx.embed();
        let encoded = fx.take_encoded();
        for word in [
            "Noteword",
            "Glacierword",
            "Zanzibarword",
            "Kiteword",
            "Barnacleword",
        ] {
            assert!(encoded_mentions(&encoded, word), "{word} must be encoded");
        }
    }

    /// AC-2 and AC-3: a rule-matched document and a document whose indexed
    /// text is over the cap are never encoded; a document whose raw file is
    /// over the cap but whose indexed text is not is encoded.
    #[test]
    fn embed_never_encodes_rule_or_cap_excluded_documents() {
        let fx = EmbedFixture::new("# Note\nNoteword.\n");
        write_exclusion_fixture(&fx);
        set_embed_rules(&fx, &["gen/**"], Some(200));
        fx.update();
        fx.embed();
        let encoded = fx.take_encoded();
        assert!(encoded_mentions(&encoded, "Glacierword"));
        assert!(
            encoded_mentions(&encoded, "Kiteword"),
            "the cap is measured on indexed text, not the raw file"
        );
        assert!(!encoded_mentions(&encoded, "Zanzibarword"), "rule-excluded");
        assert!(!encoded_mentions(&encoded, "Barnacleword"), "over the cap");

        let s = fx.store();
        assert!(!db::hash_has_any_vector(
            &s.db,
            &hash_of(&fx, "gen/skip.md")
        ));
        assert!(!db::hash_has_any_vector(&s.db, &hash_of(&fx, "big.md")));
        assert_eq!(
            rqmd_core::embed_exclusion::pending_embed_count(
                &s.db,
                &store::expected_fingerprint(),
                &store::expected_ast_fingerprints(),
            )
            .unwrap(),
            0,
            "the excluded documents are not pending"
        );
    }

    /// AC-5: a document that becomes excluded keeps its vectors through a
    /// plain embed, and `embed --cleanup` drops them while keeping the rest.
    #[test]
    fn newly_excluded_document_keeps_its_vectors_until_cleanup() {
        let fx = EmbedFixture::new("# Note\nNoteword.\n");
        fx.write("later.md", "# Later\nLaterword lighthouse text.\n");
        fx.update();
        fx.embed();
        assert!(encoded_mentions(&fx.take_encoded(), "Laterword"));
        let later = hash_of(&fx, "later.md");
        let note = hash_of(&fx, "note.md");
        let hnsw_before = fx.store().hnsw_size();

        set_embed_rules(&fx, &["later.md"], None);
        fx.embed();
        assert!(fx.take_encoded().is_empty(), "nothing to encode");
        assert!(db::hash_has_any_vector(&fx.store().db, &later), "kept");

        run_cleanup(fx.index_dir.path()).unwrap();
        let s = fx.store();
        assert!(
            !db::hash_has_any_vector(&s.db, &later),
            "dropped by cleanup"
        );
        assert!(db::hash_has_any_vector(&s.db, &note), "others kept");
        assert!(s.hnsw_size() < hnsw_before, "evicted from HNSW too");
    }
}
