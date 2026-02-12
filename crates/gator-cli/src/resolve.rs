//! Plan ID resolution and TOML write-back.
//!
//! - [`resolve_plan_id`] accepts either a UUID string or a path to a plan
//!   TOML file. If given a file, it reads the `[plan].id` field.
//! - [`write_plan_id_to_file`] uses `toml_edit` to surgically insert
//!   `id = "..."` into the `[plan]` section without disturbing comments
//!   or formatting.

use std::path::Path;

use anyhow::{Context, Result, bail};
use uuid::Uuid;

/// Determine whether `input` refers to a file path or a bare UUID, and
/// return the resolved plan UUID.
///
/// Heuristic: if the string ends with `.toml`, contains a path separator,
/// or names a file that exists on disk, treat it as a file path. Otherwise,
/// try parsing as a UUID.
pub fn resolve_plan_id(input: &str) -> Result<Uuid> {
    if looks_like_file_path(input) {
        read_plan_id_from_file(input)
    } else {
        match Uuid::parse_str(input) {
            Ok(uuid) => Ok(uuid),
            Err(uuid_err) => {
                if Path::new(input).is_file() {
                    read_plan_id_from_file(input)
                } else {
                    Err(uuid_err).with_context(|| {
                        format!("invalid plan ID: {input:?} (not a valid UUID and not a file)")
                    })
                }
            }
        }
    }
}

/// Returns true if the input string looks like a file path rather than a UUID.
fn looks_like_file_path(input: &str) -> bool {
    input.ends_with(".toml") || input.contains('/')
}

/// Read the `[plan].id` field from a TOML file.
fn read_plan_id_from_file(path: &str) -> Result<Uuid> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read plan file: {path}"))?;

    let plan_toml: gator_core::plan::PlanToml =
        toml::from_str(&content).with_context(|| format!("failed to parse TOML from {path}"))?;

    match plan_toml.plan.id {
        Some(id) => Ok(id),
        None => bail!(
            "plan file {path:?} has no id field in [plan] section.\n\
             Run `gator plan create {path}` first to register it."
        ),
    }
}

/// Insert `id = "<uuid>"` into the `[plan]` section of an existing TOML
/// file, preserving all other content including comments and formatting.
pub fn write_plan_id_to_file(path: &str, plan_id: Uuid) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;

    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {path} as TOML document"))?;

    let plan_table = doc
        .get_mut("plan")
        .and_then(|v| v.as_table_mut())
        .with_context(|| format!("{path} has no [plan] table"))?;

    // Collect existing entries, insert id first, then re-add the rest.
    let entries: Vec<(String, toml_edit::Item)> = plan_table
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    plan_table.clear();
    plan_table.insert("id", toml_edit::value(plan_id.to_string()));
    for (key, value) in entries {
        plan_table.insert(&key, value);
    }

    std::fs::write(path, doc.to_string()).with_context(|| format!("failed to write {path}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_valid_uuid() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let result = resolve_plan_id(id).unwrap();
        assert_eq!(result.to_string(), id);
    }

    #[test]
    fn resolve_invalid_uuid_no_file() {
        let result = resolve_plan_id("not-a-uuid");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_from_file_with_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.toml");
        let id = Uuid::new_v4();
        let content = format!(
            "[plan]\nid = \"{id}\"\nname = \"test\"\nbase_branch = \"main\"\n\n\
             [[tasks]]\nname = \"t1\"\ndescription = \"do it\"\nscope = \"narrow\"\ngate = \"auto\"\n"
        );
        std::fs::write(&path, &content).unwrap();
        let resolved = resolve_plan_id(path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, id);
    }

    #[test]
    fn resolve_from_file_without_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.toml");
        let content = "[plan]\nname = \"test\"\nbase_branch = \"main\"\n\n\
             [[tasks]]\nname = \"t1\"\ndescription = \"do it\"\nscope = \"narrow\"\ngate = \"auto\"\n";
        std::fs::write(&path, content).unwrap();
        let result = resolve_plan_id(path.to_str().unwrap());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no id field"));
    }

    #[test]
    fn resolve_nonexistent_file() {
        let result = resolve_plan_id("/tmp/nonexistent_gator_plan_xyz.toml");
        assert!(result.is_err());
    }

    #[test]
    fn write_plan_id_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.toml");
        let content = "# This is my plan\n[plan]\nname = \"test\"\nbase_branch = \"main\"\n\
             # token_budget = 100000\n\n[[tasks]]\nname = \"t1\"\ndescription = \"do it\"\n\
             scope = \"narrow\"\ngate = \"auto\"\n";
        std::fs::write(&path, content).unwrap();

        let id = Uuid::new_v4();
        write_plan_id_to_file(path.to_str().unwrap(), id).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains(&id.to_string()));
        assert!(result.contains("# This is my plan"));
        assert!(result.contains("# token_budget = 100000"));

        let parsed: gator_core::plan::PlanToml =
            toml::from_str(&result).expect("should parse after write-back");
        assert_eq!(parsed.plan.id, Some(id));
        assert_eq!(parsed.plan.name, "test");
    }

    #[test]
    fn looks_like_file_path_tests() {
        assert!(looks_like_file_path("plan.toml"));
        assert!(looks_like_file_path("./plan.toml"));
        assert!(looks_like_file_path("plans/my-plan.toml"));
        assert!(looks_like_file_path("/absolute/path.toml"));
        assert!(!looks_like_file_path(
            "550e8400-e29b-41d4-a716-446655440000"
        ));
    }
}
