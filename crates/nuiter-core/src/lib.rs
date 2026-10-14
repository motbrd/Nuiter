//! Nuiter's engine: everything but the windows. Reading files (`extract`), the index (`index`), search (`search`) and
//! the model runtime (`runtime`). The app (`Nuiter.exe`) and `nuiter-cli` are thin layers over it.

pub mod cancel;
pub mod cli_support;
pub mod extract;
pub mod file_kinds;
pub mod index;
pub mod memory;
pub mod net;
pub mod paths;
pub mod runtime;
pub mod search;
pub mod settings;
pub mod storage_move;
pub mod uni;
pub mod vector_math;
pub mod win;

pub use cancel::{Cancel, Cancelled};
pub use file_kinds::FileCategory;
pub use paths::{Log, Paths};
pub use settings::Settings;

/// This build's version (Cargo's), as releases name it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
