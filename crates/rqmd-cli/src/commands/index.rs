use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

use rqmd_core::{db, IndexOutcome, PendingVectorMeta};

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
    let docs_needing_embed: i64 = db::count_docs_needing_embed(&s.db).unwrap_or(0);
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

    // ── AST Chunking (qmd.ts:539-563: rqmd is regex-only so always "not available") ──
    println!("\n\x1b[1mAST Chunking\x1b[0m");
    println!("  Status:   \x1b[2mnot available\x1b[0m");

    // ── Collections (qmd.ts:565-586, per-collection multi-line blocks) ───────────
    let cols = db::list_collections(&s.db)?;
    if cols.is_empty() {
        println!(
            "\n\x1b[2mNo collections. Run 'rqmd collection add .' to index markdown files.\x1b[0m"
        );
    } else {
        println!("\n\x1b[1mCollections\x1b[0m");
        for col in &cols {
            let (count, last_mod) = db::collection_doc_stats(&s.db, &col.name).unwrap_or((0, None));
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
            if let Ok(Some(ctx)) = db::get_context_for_collection(&s.db, &col.name) {
                let preview = if ctx.len() > 60 {
                    format!("{}...", &ctx[..57])
                } else {
                    ctx.clone()
                };
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
    }

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
    let mut tips: Vec<String> = Vec::new();

    // Tip 1: collections missing context
    let without_ctx: Vec<&str> = cols
        .iter()
        .filter(|c| {
            db::get_context_for_collection(&s.db, &c.name)
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
            "Add context to collections for better search results: {names}{more}"
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

    Ok(())
}

/// Flush the HNSW file to disk, then atomically commit buffered vector metadata
/// rows to SQLite.  Called every CHECKPOINT_INTERVAL docs and at the end of embed.
///
/// Ordering guarantee: the HNSW save must succeed before any DB rows are written.
/// If interrupted between the two steps, only the HNSW is updated — the next run
/// will re-embed the un-written docs, producing new vids that continue from
/// `index.size()` (set by VectorIndex::load → next_vid = size).
fn checkpoint(s: &mut rqmd_core::Store, pending: &mut Vec<PendingVectorMeta>) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    // 1. Persist HNSW first — this is the durability barrier.
    s.flush()?;
    // 2. Write metadata rows in a single transaction.
    let tx = s.db.transaction()?;
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

pub fn run_embed(index_dir: &Path, collection: Option<&str>, rebuild: bool) -> Result<()> {
    let cols = {
        let s = store::open_store_no_backend(index_dir, true)?;
        match collection {
            Some(c) => vec![db::list_collections(&s.db)?
                .into_iter()
                .find(|col| col.name == c)
                .with_context(|| format!("collection '{c}' not found"))?],
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
        let needs_embed: i64 = db::count_docs_needing_embed(&s.db).unwrap_or(1);
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

    // Track hashes queued in this run to prevent duplicate-hash drift: multiple documents
    // with identical bodies share a hash, and embedding each copy adds a vector to HNSW
    // while the DB ON-CONFLICT UPDATE overwrites the vid — orphaning the previous vid and
    // widening the HNSW/DB gap on every run.  Deduping by hash here stops that at source.
    let mut seen_hashes: HashSet<String> = HashSet::new();

    for col in &cols {
        // Collect all docs for this collection.  We embed only those whose content
        // hash has no entry in content_vectors (incremental / resumable).
        let docs = db::list_documents(&s.db, Some(&col.name))?;
        let total = docs.len();

        // Collect only docs whose hash has no vector rows yet (incremental / resumable)
        // and whose hash has not already been queued in this run (duplicate-hash guard).
        let mut todo_indices: Vec<usize> = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            if !db::hash_has_any_vector(&s.db, &doc.hash) && !seen_hashes.contains(&doc.hash) {
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
                let pct = if todo_total > 0 {
                    (done as f64 / todo_total as f64) * 100.0
                } else {
                    100.0
                };
                let bar = fmt::render_progress_bar(pct, 30);
                let pct_int = pct.round() as u64;
                let elapsed = start.elapsed().as_secs_f64();
                let (throughput_str, eta_str) = if elapsed > 2.0 && done > 0 {
                    let bps = bytes_processed as f64 / elapsed;
                    let docs_per_sec = done as f64 / elapsed;
                    let remaining = (todo_total - done) as f64 / docs_per_sec.max(0.001);
                    (
                        format!("{}/s", fmt_bytes(bps as u64)),
                        fmt::format_eta(remaining),
                    )
                } else {
                    (".../s".to_string(), "...".to_string())
                };
                let chunks_so_far = total_new_chunks + pending.len();
                let line = format!(
                    "\x1b[36m{bar}\x1b[0m \x1b[1m{pct_int:>3}% input\x1b[0m \
                     \x1b[2m{chunks_so_far} chunks · {done}/{todo_total} docs · {throughput_str} · ETA {eta_str}\x1b[0m"
                );
                let w = fmt::term_width().unwrap_or(80).saturating_sub(1);
                eprint!("\r\x1b[2K{}", fmt::fit_to_width(&line, w));
            }

            // Embed and stage — do NOT write to DB yet.
            let new_chunks = s.embed_document_chunks(&doc.hash, &body)?;
            let chunk_count = new_chunks.len();
            pending.extend(new_chunks);
            done += 1;
            bytes_processed += body.len();
            total_new_chunks += chunk_count;

            // Checkpoint every N docs so an interrupt only re-embeds the last batch.
            if done.is_multiple_of(CHECKPOINT_INTERVAL) {
                checkpoint(&mut s, &mut pending)?;
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
    checkpoint(&mut s, &mut pending)?;

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
            Some(c) => vec![db::list_collections(&s.db)?
                .into_iter()
                .find(|col| col.name == c)
                .with_context(|| format!("collection '{c}' not found"))?],
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
    // Still needs write access: flush() unconditionally calls hnsw.save().
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

        let dir = Path::new(&col.path);
        if !dir.exists() {
            eprintln!("  WARN: directory not found: {}", dir.display());
            continue;
        }

        // Run the collection's pre-update hook (e.g. `git fetch && git pull ...`)
        // before walking the directory, so indexing sees freshly synced content.
        if let Some(cmd) = col.update_command.as_deref() {
            if !cmd.trim().is_empty() {
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
        }

        let include_set = match exclusions::build_include_set(&col.pattern) {
            Ok(set) => set,
            Err(e) => {
                eprintln!(
                    "  WARN: {}: invalid mask '{}': {e:#}",
                    col.name, col.pattern
                );
                continue;
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
    }

    // "needs embeddings" notice (qmd.ts:747–748) — printed once after all collections
    // so it isn't repeated N times with the same global count during a multi-collection update.
    let needs_embed: i64 = db::count_docs_needing_embed(&s.db).unwrap_or(0);
    if needs_embed > 0 {
        println!(
            "\nRun 'rqmd embed' to update embeddings ({needs_embed} unique hashes need vectors)"
        );
    }
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

        // Stale-embedding check: content_vectors can accumulate rows from more than
        // one embed_fingerprint after an embed-model or chunking-constant change,
        // since `rqmd embed` only re-embeds hashes with zero vectors (see
        // `count_docs_needing_embed`). A single stale fingerprint (no mixing at
        // all — e.g. right after upgrading, before any new doc has been embedded)
        // is just as broken as a mix, so the gate below checks for any mismatch
        // against the current fingerprint, not just `breakdown.len() > 1`.
        let breakdown = db::fingerprint_breakdown(&s.db).unwrap_or_default();
        let expected = store::expected_fingerprint();
        if breakdown.iter().any(|(fp, _)| fp != &expected) {
            println!("\n  \x1b[33m⚠ Stale embedding fingerprint(s) detected\x1b[0m — vectors were generated by a different model/chunking config than the one active now:");
            for (fp, count) in &breakdown {
                let marker = if *fp == expected {
                    " (current)"
                } else {
                    " (stale)"
                };
                println!("    {fp}{marker} — {count} chunks");
            }
            println!(
                "    Run 'rqmd embed --rebuild' to re-embed everything under the current model"
            );
        }

        // Orphaned-vector check: `rqmd update` soft-deletes documents whose file was
        // removed or renamed (active=0) rather than hard-deleting, so their
        // content_vectors/HNSW vids survive but become unreachable — every
        // vector→document join requires an active document. Say this plainly rather
        // than implying the space is freed; only `embed --rebuild` reclaims it.
        let orphaned_vectors = db::count_orphaned_vectors(&s.db).unwrap_or(0);
        if orphaned_vectors > 0 {
            println!(
                "\n  \x1b[33m⚠ {orphaned_vectors} orphaned vector(s)\x1b[0m — left behind by documents removed or renamed since their last embed. Unreachable in search but not yet freed from the HNSW file."
            );
            println!("    Run 'rqmd embed --rebuild' to reclaim the space");
        }

        // Recommended next steps.
        let needs_embed: i64 = db::count_docs_needing_embed(&s.db).unwrap_or(0);
        if needs_embed > 0 {
            println!("\n  Recommended next step");
            println!("    Run 'rqmd embed' to generate embeddings ({needs_embed} hashes pending)");
        }
    }
    Ok(())
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
