use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod format;
mod store;

/// qmd — hybrid local document search
#[derive(Parser)]
#[command(name = "rqmd", version, about, long_about = None)]
struct Cli {
    /// Override the index directory (default: ~/.cache/rqmd/)
    #[arg(long, env = "RRQMD_INDEX_DIR", global = true)]
    index_dir: Option<String>,

    /// Inference backend: llama (default, GGUF via llama-cpp-2) or ort (ONNX Runtime)
    #[arg(long, env = "RRQMD_INFERENCE_BACKEND", global = true)]
    backend: Option<String>,

    /// ORT execution provider: auto (default), coreml, cuda, directml, cpu
    #[arg(long, env = "RRQMD_ORT_EP", global = true)]
    ort_ep: Option<String>,

    /// Show native model-loading and inference logs (also enabled by RUST_LOG)
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Hybrid search: BM25 + vector + rerank (recommended)
    Query {
        query: String,
        #[arg(short = 'c', long)]
        collection: Option<String>,
        #[arg(short = 'n', default_value = "10")]
        num: usize,
        #[arg(long, default_value = "cli")]
        format: String,
        #[arg(long)]
        no_rerank: bool,
        #[arg(long)]
        full: bool,
    },
    /// Full-text keyword search (BM25 only, no LLM)
    Search {
        query: String,
        #[arg(short = 'c', long)]
        collection: Option<String>,
        #[arg(short = 'n', default_value = "10")]
        num: usize,
        #[arg(long, default_value = "cli")]
        format: String,
        #[arg(long)]
        full: bool,
    },
    /// Vector similarity search (no rerank)
    Vsearch {
        query: String,
        #[arg(short = 'c', long)]
        collection: Option<String>,
        #[arg(short = 'n', default_value = "10")]
        num: usize,
        #[arg(long, default_value = "cli")]
        format: String,
        #[arg(long)]
        full: bool,
    },
    /// Get document by path or docid (#abc123)
    Get {
        path: String,
        #[arg(short = 'l', long)]
        max_lines: Option<usize>,
        #[arg(long)]
        no_line_numbers: bool,
        #[arg(long, default_value = "cli")]
        format: String,
    },
    /// Get multiple documents by glob or comma-separated list
    #[command(name = "multi-get")]
    MultiGet {
        pattern: String,
        #[arg(short = 'c', long)]
        collection: Option<String>,
        #[arg(short = 'l', long)]
        max_lines: Option<usize>,
        #[arg(long, default_value = "cli")]
        format: String,
    },
    /// List collections or files in a collection
    Ls {
        /// Collection or collection/path prefix
        path: Option<String>,
    },
    /// Collection management
    #[command(subcommand)]
    Collection(CollectionCommand),
    /// Context management
    #[command(subcommand)]
    Context(ContextCommand),
    /// Create a project-local .qmd index
    Init,
    /// Show index status and collections
    Status,
    /// Generate vector embeddings (requires models)
    Embed {
        #[arg(short = 'c', long)]
        collection: Option<String>,
    },
    /// Re-index all collections
    Update {
        #[arg(short = 'c', long)]
        collection: Option<String>,
    },
    /// Diagnose config, index, model, and device issues
    Doctor,
    /// Benchmark embed throughput for the configured backend
    Bench {
        /// Number of benchmark rounds (default: 5)
        #[arg(short = 'n', long, default_value = "5")]
        rounds: usize,
    },
    /// Search quality evaluation against synthetic fixtures
    Eval {
        /// Search mode: bm25 (default, no model), vec, hybrid
        #[arg(long, default_value = "bm25")]
        mode: String,
        /// Print per-query pass/fail breakdown
        #[arg(long)]
        verbose: bool,
    },
    /// Start the MCP server (stdio by default; --http for Streamable HTTP)
    Mcp {
        /// Serve over Streamable HTTP instead of stdio
        #[arg(long)]
        http: bool,
        /// HTTP port (default: 8181)
        #[arg(long, default_value = "8181")]
        port: u16,
    },
}

#[derive(Subcommand)]
pub enum CollectionCommand {
    /// Add a directory as a collection
    Add {
        path: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        mask: Option<String>,
    },
    /// List all collections
    #[command(alias = "ls")]
    List,
    /// Remove a collection
    #[command(alias = "rm")]
    Remove { name: String },
    /// Rename a collection
    #[command(alias = "mv")]
    Rename { old: String, new: String },
    /// Show collection details
    #[command(alias = "info")]
    Show { name: String },
    /// Set the pre-update command hook
    #[command(name = "update-cmd", alias = "set-update")]
    UpdateCmd { name: String, cmd: Option<String> },
    /// Include collection in default (unscoped) queries
    Include { name: String },
    /// Exclude collection from default (unscoped) queries
    Exclude { name: String },
}

#[derive(Subcommand)]
pub enum ContextCommand {
    /// Add context for a path
    Add { path: Option<String>, text: String },
    /// List all contexts
    List,
    /// Remove context for a path
    #[command(alias = "remove")]
    Rm { path: String },
    /// Check for paths missing context
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Propagate CLI flags into env vars so BackendKind::from_env() picks them up.
    if let Some(b) = &cli.backend {
        std::env::set_var("RRQMD_INFERENCE_BACKEND", b);
    }
    if let Some(ep) = &cli.ort_ep {
        std::env::set_var("RRQMD_ORT_EP", ep);
    }

    // Install a tracing subscriber.  Default behaviour mirrors qmd: native llama.cpp /
    // ggml logs are silent unless the user explicitly opts in via --verbose or RUST_LOG.
    //
    // Default filter (no --verbose, no RUST_LOG):
    //   warn,llama-cpp-2=error,ggml=error
    //
    // Rationale: benign WARNs from llama.cpp ("control-looking token", "n_ctx_seq <
    // n_ctx_train") have no actionable fix; hiding them keeps the output as clean as the
    // original TypeScript qmd.  ERROR-level messages from those crates still surface.
    // --verbose (or RUST_LOG) restores full output.
    let rust_log_set = std::env::var("RUST_LOG").is_ok();
    if !rust_log_set {
        std::env::set_var(
            "RUST_LOG",
            if cli.verbose {
                "debug"
            } else {
                "warn,llama-cpp-2=error,ggml=error"
            },
        );
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    if cli.verbose || rust_log_set {
        // Signal to library crates (e.g. rqmd-llm) that verbose mode is on so they
        // can adjust their own native-library log levels accordingly.
        std::env::set_var("RRQMD_VERBOSE", "1");
    }

    let index_dir = store::resolve_index_dir(cli.index_dir.as_deref())?;

    match cli.command {
        Commands::Query {
            query,
            collection,
            num,
            format,
            no_rerank,
            full,
        } => commands::query::run_query(
            &index_dir,
            &query,
            collection.as_deref(),
            num,
            &format,
            no_rerank,
            full,
        ),
        Commands::Search {
            query,
            collection,
            num,
            format,
            full,
        } => commands::query::run_search(
            &index_dir,
            &query,
            collection.as_deref(),
            num,
            &format,
            full,
        ),
        Commands::Vsearch {
            query,
            collection,
            num,
            format,
            full,
        } => commands::query::run_vsearch(
            &index_dir,
            &query,
            collection.as_deref(),
            num,
            &format,
            full,
        ),
        Commands::Get {
            path,
            max_lines,
            no_line_numbers,
            format,
        } => commands::get::run_get(&index_dir, &path, max_lines, no_line_numbers, &format),
        Commands::MultiGet {
            pattern,
            collection,
            max_lines,
            format,
        } => commands::get::run_multi_get(
            &index_dir,
            &pattern,
            collection.as_deref(),
            max_lines,
            &format,
        ),
        Commands::Ls { path } => commands::get::run_ls(&index_dir, path.as_deref()),
        Commands::Collection(cmd) => commands::collection::run(&index_dir, cmd),
        Commands::Context(cmd) => commands::context::run(&index_dir, cmd),
        Commands::Init => commands::index::run_init(),
        Commands::Status => commands::index::run_status(&index_dir),
        Commands::Embed { collection } => {
            commands::index::run_embed(&index_dir, collection.as_deref())
        }
        Commands::Update { collection } => {
            commands::index::run_update(&index_dir, collection.as_deref())
        }
        Commands::Doctor => commands::index::run_doctor(&index_dir),
        Commands::Bench { rounds } => commands::bench::run_bench(&index_dir, rounds),
        Commands::Eval { mode, verbose } => commands::eval::run_eval(&index_dir, &mode, verbose),
        Commands::Mcp { http, port } => commands::mcp::run_mcp(&index_dir, http, port),
    }
}
