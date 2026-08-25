pub mod chunking;
pub mod db;
pub mod fts;
pub mod hnsw;
pub mod query;
pub mod resolve;
pub mod rrf;
pub mod store;
pub mod types;

pub use chunking::{SnippetResult, extract_snippet, snap_char_boundary_backward};
pub use store::{IndexOutcome, PendingVectorMeta, Store, StoreConfig};
pub use types::{Collection, Document, SearchResult};
