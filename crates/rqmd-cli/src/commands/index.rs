use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

use rqmd_core::{Collection, IndexOutcome, PendingVectorMeta, db, snap_char_boundary_backward};

use crate::{document, exclusions, format as fmt, store};

pub fn run_status(index_dir: &Path) -> Result<()> {
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
    let docs_needing_embed: i64 =
        db::count_docs_needing_embed(&s.db, &store::expected_fingerprint()).unwrap_or(0);
    let last_modified: Option<String> =
        s.db.query_row(
            "SELECT MAX(modified_at) FROM documents WHERE active=1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(None);

    // ── Header (qmd.ts:492 style, rqmd branding) ────────────────────────────────
    println!("\x1b[1mRQMD Status (Rust engine)\x1b[0m\n");
    println!("Index: {}", index_dir.display());
    println!("Size:  {}", fmt_bytes(total_index_size));
    println!();

    // ── Documents (qmd.ts:513-521) ───────────────────────────────────────────────
    println!("\x1b[1mDocuments\x1b[0m");
    println!("  Total:    {total_docs} files indexed");
    println!("  Vectors:  {total_vecs} embedded");
    if docs_needing_embed > 0 {
        println!(
            "  \x1b[33mPending:  {docs_needing_embed} need embedding\x1b[0m (run 'rqmd embed')"
        );
    }
    if let Some(ref ts) = last_modified {
        println!("  Updated:  {}", fmt::format_time_ago(ts));
    }

    // ── AST Chunking (qmd.ts:539-563) ─────────────────────────────────────────────
    println!("\n\x1b[1mAST Chunking\x1b[0m");
    if rqmd_core::chunking::ast_chunking_compiled() {
        let exts = rqmd_core::chunking::ast_chunking_extensions().join(", ");
        println!("  Status:   \x1b[32menabled\x1b[0m");
        println!("  Grammars: {exts}");
    } else {
        println!("  Status:   \x1b[2mnot available\x1b[0m (build with --features ast-chunking)");
    }

    // ── Collections (qmd.ts:565-586, per-collection multi-line blocks) ───────────
    let cols = db::list_collections(&s.db)?;
    print_status_collections(&s.db, &cols)?;

    // ── Models (qmd.ts:606-617) — show repo (browsable) + exact downloaded file ──
    println!("\n\x1b[1mModels\x1b[0m");
    println!(
        "  Embedding:   https://huggingface.co/{}",
        rqmd_llm::DEFAULT_EMBED_REPO
    );
    println!(
        "               \x1b[2m└─ {}\x1b[0m",
        rqmd_llm::DEFAULT_EMBED_FILE
    );
    println!(
        "  Reranking:   https://huggingface.co/{}",
        rqmd_llm::DEFAULT_RERANK_REPO
    );
    println!(
        "               \x1b[2m└─ {}\x1b[0m",
        rqmd_llm::DEFAULT_RERANK_FILE
    );
    println!(
        "  Generation:  https://huggingface.co/{}",
        rqmd_llm::DEFAULT_GENERATE_REPO
    );
    println!(
        "               \x1b[2m└─ {}\x1b[0m",
        rqmd_llm::DEFAULT_GENERATE_FILE
    );

    // ── Tips (qmd.ts:621-654) ────────────────────────────────────────────────────
    print_status_tips(&s.db, &cols);

    Ok(())
}

/// Print the "Collections" and "Examples" blocks of `rqmd status`
/// (qmd.ts:565-601).
fn print_status_collections(conn: &Connection, cols: &[Collection]) -> Result<()> {
    if cols.is_empty() {
        println!(
            "\n\x1b[2mNo collections. Run 'rqmd collection add .' to index markdown files.\x1b[0m"
        );
        return Ok(());
    }

    println!("\n\x1b[1mCollections\x1b[0m");
    for col in cols {
        let (count, last_mod) = db::collection_doc_stats(conn, &col.name).unwrap_or((0, None));
        let last_mod_str = last_mod
            .as_deref()
            .map(fmt::format_time_ago)
            .unwrap_or_else(|| "never".to_string());
        println!(
            "  \x1b[36m{}\x1b[0m \x1b[2m(rqmd://{}/)\x1b[0m",
            col.name, col.name
        );
        println!("    \x1b[2mPattern:\x1b[0m  {}", col.pattern);
        println!("    \x1b[2mFiles:\x1b[0m    {count} (updated {last_mod_str})");
        if let Ok(Some(ctx)) = db::get_context_for_collection(conn, &col.name) {
            let preview = truncate_context_preview(&ctx);
            println!("    \x1b[2mContexts:\x1b[0m 1");
            println!("      \x1b[2m/:\x1b[0m {preview}");
        }
    }

    // ── Examples (qmd.ts:588-601, using rqmd command names) ─────────────────
    println!("\n\x1b[1mExamples\x1b[0m");
    println!("  \x1b[2m# List files in a collection\x1b[0m");
    if let Some(first) = cols.first() {
        println!("  rqmd ls {}", first.name);
    }
    println!("  \x1b[2m# Get a document\x1b[0m");
    if let Some(first) = cols.first() {
        println!("  rqmd get rqmd://{}/path/to/file.md", first.name);
    }
    println!("  \x1b[2m# Search within a collection\x1b[0m");
    if let Some(first) = cols.first() {
        println!("  rqmd search \"query\" -c {}", first.name);
    }
    Ok(())
}

/// Build and print the "Tips" block of `rqmd status` (qmd.ts:621-654).
fn print_status_tips(conn: &Connection, cols: &[Collection]) {
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
        tips.push(
            "  \x1b[2mrqmd context add rqmd://<name>/ \"What this collection contains\"\x1b[0m"
                .to_string(),
        );
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
            tips.push(
                "  \x1b[2mrqmd collection update-cmd <name> 'git pull --rebase --ff-only'\x1b[0m"
                    .to_string(),
            );
        }
    }

    if !tips.is_empty() {
        println!("\n\x1b[1mTips\x1b[0m");
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
    // 2. Write metadata rows in a single transaction.
    let tx = s.db.transaction()?;
    for hash in pending_deletes.drain() {
        db::delete_vectors_for_hash(&tx, &hash).context("delete stale vectors for hash")?;
    }
    for m in pending.drain(..) {
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
    }
    tx.commit().context("commit vector metadata")?;
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
) -> String {
    let pct = if todo_total > 0 {
        (done as f64 / todo_total as f64) * 100.0
    } else {
        100.0
    };
    let bar = fmt::render_progress_bar(pct, 30);
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
        "\x1b[36m{bar}\x1b[0m \x1b[1m{pct_int:>3}% input\x1b[0m \
         \x1b[2m{chunks_so_far} chunks · {done}/{todo_total} docs · {throughput_str} · ETA {eta_str}\x1b[0m"
    )
}

pub fn run_embed(index_dir: &Path, collection: Option<&str>, rebuild: bool) -> Result<()> {
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
        let hnsw_path = store::store_config(index_dir, true).hnsw_path;
        if hnsw_path.exists() {
            std::fs::remove_file(&hnsw_path)
                .with_context(|| format!("remove hnsw file: {}", hnsw_path.display()))?;
        }
        let s = store::open_store_no_backend(index_dir, true)?;
        match collection {
            Some(c) => {
                db::clear_vectors_for_collection(&s.db, c)
                    .context("clear vectors for collection")?;
            }
            None => {
                db::clear_all_vectors(&s.db).context("clear all vectors")?;
            }
        }
        eprintln!(
            "\x1b[33mrqmd: rebuild mode — cleared {} vectors; re-embedding from scratch\x1b[0m",
            if collection.is_some() {
                "collection"
            } else {
                "all"
            }
        );
    } else {
        // Fast path: nothing to do.
        let s = store::open_store_no_backend(index_dir, true)?;
        let needs_embed: i64 =
            db::count_docs_needing_embed(&s.db, &store::expected_fingerprint()).unwrap_or(1);
        if needs_embed == 0 {
            println!("\x1b[32m✓ All content hashes already have embeddings.\x1b[0m");
            return Ok(());
        }
    }

    let mut s = store::open_store_with_backend(index_dir, false)?;

    // Stale-fingerprint advisory: --rebuild already cleared every vector row above,
    // so there is nothing to compare yet; only warn on the incremental path, where
    // a model/chunking change leaves existing vectors under the old fingerprint
    // untouched (incremental embed only fills in hashes with zero vectors).
    if !rebuild {
        store::warn_if_fingerprint_stale(&s);
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
                "\x1b[33mrqmd: warning: vector index out of sync ({hnsw_size} indexed \
                 vs {db_vec_count} expected); run `rqmd embed --rebuild` to repair.\x1b[0m"
            );
        }
    }
    let is_tty = fmt::atty_stderr();
    let start = Instant::now();

    let mut total_new_docs = 0usize;
    let mut total_new_chunks = 0usize;

    // Buffer for pending vector metadata — flushed every CHECKPOINT_INTERVAL docs.
    let mut pending: Vec<PendingVectorMeta> = Vec::new();
    // Hashes whose stale content_vectors rows must be deleted at the next checkpoint,
    // superseding vectors embedded under a since-changed fingerprint (see `checkpoint`).
    let mut pending_deletes: HashSet<String> = HashSet::new();

    // Track hashes queued in this run to prevent duplicate-hash drift: multiple documents
    // with identical bodies share a hash, and embedding each copy adds a vector to HNSW
    // while the DB ON-CONFLICT UPDATE overwrites the vid — orphaning the previous vid and
    // widening the HNSW/DB gap on every run.  Deduping by hash here stops that at source.
    let mut seen_hashes: HashSet<String> = HashSet::new();

    let fingerprint = store::expected_fingerprint();

    for col in &cols {
        // Collect all docs for this collection.  We embed only those whose content
        // hash has no vector row at the current fingerprint (incremental / resumable;
        // also catches hashes whose only vectors are stale, so a chunker/model change
        // gets re-embedded instead of silently skipped).
        let docs = db::list_documents(&s.db, Some(&col.name))?;
        let total = docs.len();

        // Collect only docs whose hash has no vector row at the current fingerprint yet
        // and whose hash has not already been queued in this run (duplicate-hash guard).
        let mut todo_indices: Vec<usize> = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            if !db::hash_has_vector_with_fingerprint(&s.db, &doc.hash, &fingerprint)
                && !seen_hashes.contains(&doc.hash)
            {
                seen_hashes.insert(doc.hash.clone());
                todo_indices.push(i);
            }
        }

        let todo_total = todo_indices.len();
        if todo_total == 0 {
            continue;
        }

        let mut done = 0usize;
        let mut bytes_processed = 0usize;
        for idx in &todo_indices {
            let doc = &docs[*idx];
            let body = db::get_content(&s.db, &doc.hash)?.unwrap_or_default();
            if body.is_empty() {
                continue;
            }

            if is_tty {
                let line = render_embed_progress_line(
                    done,
                    todo_total,
                    total_new_chunks + pending.len(),
                    bytes_processed,
                    start.elapsed().as_secs_f64(),
                );
                let w = fmt::term_width().unwrap_or(80).saturating_sub(1);
                eprint!("\r\x1b[2K{}", fmt::fit_to_width(&line, w));
            }

            // This hash has vectors, just not at the current fingerprint (the
            // todo-selection check above is fingerprint-aware) — supersede them:
            // evict the old vids from HNSW now, and queue the DB rows for delete
            // in the same transaction as the new rows' insert (see `checkpoint`).
            if db::hash_has_any_vector(&s.db, &doc.hash) {
                let stale_vids = db::vids_for_hash(&s.db, &doc.hash)?;
                s.evict_hnsw_vectors(&stale_vids)?;
                pending_deletes.insert(doc.hash.clone());
            }

            // Embed and stage — do NOT write to DB yet.
            let new_chunks = s.embed_document_chunks(&doc.hash, &doc.path, &body)?;
            let chunk_count = new_chunks.len();
            pending.extend(new_chunks);
            done += 1;
            bytes_processed += body.len();
            total_new_chunks += chunk_count;

            // Checkpoint every N docs so an interrupt only re-embeds the last batch.
            if done.is_multiple_of(CHECKPOINT_INTERVAL) {
                checkpoint(&mut s, &mut pending, &mut pending_deletes)?;
            }
        }

        total_new_docs += done;

        // Collection done — any remaining rows come after the outer loop's final checkpoint.
        let _total = total; // suppress unused warning
    }

    // Final 100% bar before the summary line.
    if is_tty {
        let bar = fmt::render_progress_bar(100.0, 30);
        let line = format!("\x1b[32m{bar}\x1b[0m \x1b[1m100% input\x1b[0m");
        let w = fmt::term_width().unwrap_or(80).saturating_sub(1);
        eprint!("\r\x1b[2K{}", fmt::fit_to_width(&line, w));
    }

    // Final checkpoint for any remaining pending rows.
    checkpoint(&mut s, &mut pending, &mut pending_deletes)?;

    // Summary — matches qmd's "✓ Done!" line (qmd.ts:1938).
    let elapsed = fmt::format_eta(start.elapsed().as_secs_f64());
    println!(
        "\n\x1b[32m✓ Done!\x1b[0m Embedded \x1b[1m{total_new_chunks}\x1b[0m chunks from \x1b[1m{total_new_docs}\x1b[0m documents in \x1b[1m{elapsed}\x1b[0m"
    );
    Ok(())
}

pub fn run_update(index_dir: &Path, collection: Option<&str>) -> Result<()> {
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
    let is_tty = fmt::atty_stderr();

    // Mirror qmd's "Updating N collection(s)..." header (qmd.ts:675).
    println!("\x1b[1mUpdating {} collection(s)...\x1b[0m\n", cols.len());

    for (ci, col) in cols.iter().enumerate() {
        // Per-collection header: [i/n] name (pattern)
        println!(
            "\x1b[36m[{}/{}]\x1b[0m \x1b[1m{}\x1b[0m \x1b[2m({})\x1b[0m",
            ci + 1,
            cols.len(),
            col.name,
            col.pattern
        );

        update_one_collection(&mut s, col, is_tty)?;
    }

    // "needs embeddings" notice (qmd.ts:747–748) — printed once after all collections
    // so it isn't repeated N times with the same global count during a multi-collection update.
    let needs_embed: i64 =
        db::count_docs_needing_embed(&s.db, &store::expected_fingerprint()).unwrap_or(0);
    if needs_embed > 0 {
        println!(
            "\nRun 'rqmd embed' to update embeddings ({needs_embed} unique hashes need vectors)"
        );
    }
    Ok(())
}

/// Re-walk one collection's directory, run its update hook, re-index changed
/// files, and prune deleted ones. Extracted from `run_update`'s per-collection
/// loop body; the `IndexOutcome`-based tally counters and every WARN-on-failure
/// path are preserved bit-for-bit.
fn update_one_collection(s: &mut rqmd_core::Store, col: &Collection, is_tty: bool) -> Result<()> {
    let dir = Path::new(&col.path);
    if !dir.exists() {
        eprintln!("  WARN: directory not found: {}", dir.display());
        return Ok(());
    }

    // Run the collection's pre-update hook (e.g. `git fetch && git pull ...`)
    // before walking the directory, so indexing sees freshly synced content.
    if let Some(cmd) = col.update_command.as_deref()
        && !cmd.trim().is_empty()
    {
        println!("  \x1b[2m$ {cmd}\x1b[0m");
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .status()
        {
            Ok(status) if !status.success() => {
                eprintln!("  WARN: update hook exited with {status}");
            }
            Err(e) => {
                eprintln!("  WARN: update hook failed to run: {e}");
            }
            _ => {}
        }
    }

    let include_set = match exclusions::build_include_set(&col.pattern) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "  WARN: {}: invalid mask '{}': {e:#}",
                col.name, col.pattern
            );
            return Ok(());
        }
    };
    let ignore_set = exclusions::build_ignore_set(&col.ignore);

    let mut new_count = 0usize;
    let mut updated_count = 0usize;
    let mut unchanged_count = 0usize;
    let mut processed = 0usize;
    let mut skips = document::SkipCounts::default();

    // Pre-collect matching paths so we know the total before indexing begins,
    // enabling "Indexing: N/total" progress (matching qmd's output). Reuses the
    // same walk/filter logic `collection add` uses, so a re-index can't silently
    // disagree with the original add about which files belong in the collection.
    let files = document::collect_candidates(dir, &include_set, &ignore_set, col.allow_hidden);
    let total = files.len();
    if total == 0 {
        eprintln!(
            "  WARN: mask '{}' matched 0 files under {} (try --hidden if content lives under a dot-directory)",
            col.pattern,
            dir.display()
        );
    }

    for path in &files {
        let doc = match document::prepare(path, dir) {
            Ok(doc) => doc,
            Err(reason) => {
                skips.record(reason);
                processed += 1;
                continue;
            }
        };

        processed += 1;
        if is_tty {
            let line = format!("Indexing: {processed}/{total} {}", doc.rel_path);
            let w = fmt::term_width().unwrap_or(80).saturating_sub(1);
            eprint!("\r\x1b[2K{}", fmt::fit_to_width(&line, w));
        }

        match s.index_document_fts_only_with_raw(
            &col.name,
            &doc.rel_path,
            &doc.title,
            &doc.indexed_text,
            &doc.raw,
        ) {
            Err(e) => eprintln!("  WARN: {}: {e:#}", doc.rel_path),
            Ok(IndexOutcome::New) => new_count += 1,
            Ok(IndexOutcome::Updated) => updated_count += 1,
            Ok(IndexOutcome::Unchanged) => unchanged_count += 1,
        }
    }

    // Prune documents whose file is no longer on disk (deleted or renamed away).
    // Built from `files` — the raw walked candidate list — not from the set of
    // docs that made it through `prepare` above: a file that still exists but
    // failed to read this run (permission denied, transient I/O error) is still
    // *present* and must not be soft-deleted just because this pass couldn't
    // read it. Skipped entirely when the mask matched 0 files (warned above) —
    // a config/mount glitch must never read as "delete everything".
    let removed_count = if total > 0 {
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

        // A removed document's hash may still be in active use by another
        // document — content is deduplicated globally by hash — so only
        // reclaim hashes with no remaining active reference anywhere.
        let candidate_hashes = db::hashes_for_paths(&s.db, &col.name, &removed)
            .context("look up hashes for removed documents")?;
        let mut orphaned_hashes = Vec::new();
        for hash in candidate_hashes {
            if !db::hash_referenced_by_active_document(&s.db, &hash)? {
                let vids = db::vids_for_hash(&s.db, &hash)?;
                s.evict_hnsw_vectors(&vids)?;
                orphaned_hashes.push(hash);
            }
        }
        if !orphaned_hashes.is_empty() {
            // Flush is the durability barrier: HNSW must be persisted before
            // the DB rows pointing at those vids are deleted.
            s.flush()?;
            let tx = s.db.transaction()?;
            for hash in &orphaned_hashes {
                db::delete_vectors_for_hash(&tx, hash).context("delete orphaned vectors")?;
            }
            tx.commit().context("commit orphaned vector cleanup")?;
        }

        removed.len()
    } else {
        0
    };

    s.flush()?;

    if is_tty {
        eprint!("\r\x1b[2K");
    }

    // Summary line matching qmd's "Indexed: X new, Y updated..." (qmd.ts:735), extended
    // with an honest skip-reason breakdown instead of silently dropping unreadable files.
    let skip_suffix = if skips.total() > 0 {
        format!(", skipped {} ({})", skips.total(), skips.describe())
    } else {
        String::new()
    };
    println!(
        "\nIndexed: {new_count} new, {updated_count} updated, {unchanged_count} unchanged, {removed_count} removed{skip_suffix}"
    );
    Ok(())
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
            "NO — run any rqmd command to create"
        }
    );
    println!("  Tantivy dir:   {}", index_dir.join("tantivy").display());
    println!(
        "  HNSW file:     {}",
        index_dir.join("hnsw.usearch").display()
    );
    println!();

    // Check models cache — delegate to rqmd-llm so the path and repo IDs always
    // match what the downloader uses, and HF_HOME / HF_HUB_CACHE are honoured.
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
            let count = db::list_documents(&s.db, Some(&col.name))?.len();
            println!("    {} — {count} docs at {}", col.name, col.path);
        }

        print_doctor_stale_fingerprint_check(&s.db);
        print_doctor_orphaned_vector_check(&s.db);

        // Recommended next steps.
        let needs_embed: i64 =
            db::count_docs_needing_embed(&s.db, &store::expected_fingerprint()).unwrap_or(0);
        if needs_embed > 0 {
            println!("\n  Recommended next step");
            println!("    Run 'rqmd embed' to generate embeddings ({needs_embed} hashes pending)");
        }
    }
    Ok(())
}

/// Stale-embedding check for `rqmd doctor`: an interrupted or not-yet-run
/// incremental `rqmd embed` can leave `content_vectors` rows under more than
/// one `embed_fingerprint` after an embed-model or chunking-strategy change —
/// `rqmd embed` supersedes a hash's stale rows as it re-embeds each one, but
/// until that finishes, both fingerprints coexist. A single stale fingerprint
/// (no mixing at all — e.g. right after upgrading, before `rqmd embed` has
/// run) is just as broken as a mix, so the gate below checks for any mismatch
/// against the current fingerprint, not just `breakdown.len() > 1`.
fn print_doctor_stale_fingerprint_check(conn: &Connection) {
    let breakdown = db::fingerprint_breakdown(conn).unwrap_or_default();
    let expected = store::expected_fingerprint();
    if breakdown.iter().any(|(fp, _)| fp != &expected) {
        println!(
            "\n  \x1b[33m⚠ Stale embedding fingerprint(s) detected\x1b[0m — vectors were generated by a different model/chunking config than the one active now:"
        );
        for (fp, count) in &breakdown {
            let marker = if *fp == expected {
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
/// this plainly rather than implying the space is freed; only `embed
/// --rebuild` reclaims it.
fn print_doctor_orphaned_vector_check(conn: &Connection) {
    let orphaned_vectors = db::count_orphaned_vectors(conn).unwrap_or(0);
    if orphaned_vectors > 0 {
        println!(
            "\n  \x1b[33m⚠ {orphaned_vectors} orphaned vector(s)\x1b[0m — left behind by documents removed or renamed since their last embed. Unreachable in search but not yet freed from the HNSW file."
        );
        println!("    Run 'rqmd embed --rebuild' to reclaim the space");
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

    /// End-to-end regression for the orphan-vector cleanup wired into
    /// `update_one_collection`'s prune block (not just the `rqmd-core` helpers
    /// it calls) — drives the real walk + prune path so a wiring bug (wrong
    /// variable, skipped transaction, wrong flush ordering) would be caught
    /// here even though `rqmd-core`'s own integration test only exercises the
    /// helpers directly.
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
        };
        let coll_col = Collection {
            name: "coll".into(),
            path: coll_src.path().to_string_lossy().to_string(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            include_by_default: true,
            update_command: None,
            allow_hidden: false,
        };
        db::upsert_collection(&s.db, &keep_col).unwrap();
        db::upsert_collection(&s.db, &coll_col).unwrap();

        // Initial walk indexes both collections' files as active documents.
        update_one_collection(&mut s, &keep_col, false).unwrap();
        update_one_collection(&mut s, &coll_col, false).unwrap();

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
        update_one_collection(&mut s, &coll_col, false).unwrap();

        assert!(
            db::hash_has_any_vector(&s.db, &shared_hash),
            "shared_hash's vector must survive — 'keep' still actively references it"
        );
        assert!(
            !db::hash_has_any_vector(&s.db, &unique_hash),
            "unique_hash's vector must be reclaimed — no active document references it anymore"
        );
    }
}
