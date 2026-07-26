pub mod debounce;
pub mod freshness;
pub mod lock;
pub mod pipeline;
pub mod reconcile;
pub mod update;
pub mod watcher;

pub use debounce::{Debouncer, DEBOUNCE_WINDOW};
pub use freshness::{effective_freshness, get_freshness, set_freshness, Freshness};
pub use lock::{role_line, WriterLock};
pub use reconcile::{reconcile, ReconcileReport};
pub use update::{update_file, UpdateOutcome};
pub use watcher::{spawn_watcher, spawn_writer};
