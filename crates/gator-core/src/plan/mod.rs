//! Plan management: TOML parsing, service layer, materialization, generation.

pub mod generate;
pub mod materialize;
pub mod parser;
pub mod service;
pub mod toml_format;

pub use generate::{
    GenerateContext, GenerateValidationError, InvariantInfo, build_meta_plan, build_system_prompt,
    detect_context, invariants_from_presets, validate_generated_plan,
};
pub use materialize::{materialize_plan, materialize_task};
pub use parser::{PlanParseError, parse_plan_toml};
pub use service::{create_plan_from_toml, get_plan_with_tasks};
pub use toml_format::{PlanMeta, PlanToml, TaskToml};
