//! The index: the SQLite store (`store`), the in-memory vectors (`vectors`), the indexer that keeps both in sync with
//! the folders (`indexer`), estimates of what indexing takes (`forecast`), and the guard on in-process reading
//! (`watchdog`).

pub mod forecast;
pub mod indexer;
pub mod power;
pub mod store;
pub mod vectors;
pub mod watcher;
pub mod watchdog;
