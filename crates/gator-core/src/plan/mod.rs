//! Plan management: TOML parsing, service layer, materialization.

pub mod materialize;
pub mod parser;
pub mod service;
pub mod toml_format;

pub use materialize::{materialize_plan, materialize_task};
pub use parser::{parse_plan_toml, PlanParseError};
pub use service::{create_plan_from_toml, get_plan_with_tasks};
pub use toml_format::{PlanMeta, PlanToml, TaskToml};
