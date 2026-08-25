//! Domain types and invariants shared by all TerraTranslate frontends and backends.

mod ingestion;
mod merge;
mod model;
mod processor;
mod prompt;

pub use ingestion::*;
pub use merge::{ContextConflict, ContextField, MergePlan, plan_context_merge};
pub use model::*;
pub use processor::*;
pub use prompt::{PromptData, PromptTemplate, PromptTemplateError, PromptText, render_prompt};
