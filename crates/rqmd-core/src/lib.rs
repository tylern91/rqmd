#[cfg(feature = "ast-registry")]
mod ast_chunk;
pub mod chunking;
mod cjk_tokenizer;
pub mod db;
pub mod deadline;
pub mod embed_exclusion;
pub mod eval_metrics;
pub mod frontmatter;
pub mod fts;
pub mod hnsw;
pub mod lock;
pub mod query;
pub mod resolve;
pub mod rrf;
pub mod store;
pub mod types;

pub use chunking::{SnippetResult, extract_snippet, snap_char_boundary_backward};
pub use deadline::{Deadline, TimedOut};
pub use lock::{DEFAULT_STALE_AFTER, Holder, IndexLock, LockState, lock_state, remove_lock};
pub use store::{IndexOutcome, PendingVectorMeta, RenameSummary, Store, StoreConfig};
pub use types::{
    Collection, DEFAULT_RECENCY_FLOOR, Document, MAX_HALF_LIFE, MAX_PER_FILE, QueryOutcome,
    QueryStage, Recency, RerankStatus, SearchOptions, SearchResult, clamp_per_file,
    parse_half_life, section_numbers,
};
