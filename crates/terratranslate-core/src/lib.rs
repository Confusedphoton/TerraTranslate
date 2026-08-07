//! Domain types and invariants shared by all TerraTranslate frontends and backends.

mod ingestion;
mod merge;
mod model;
mod processor;

pub use ingestion::*;
pub use merge::{ContextConflict, ContextField, MergePlan, plan_context_merge};
pub use model::*;
pub use processor::*;
