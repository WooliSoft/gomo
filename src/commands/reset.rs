use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache;
use crate::commands::{CommandOutput, OutputOptions};
use crate::workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetRequest {
    pub(crate) only_cache: bool,
}

pub(crate) fn run(
    cwd: &Path,
    request: ResetRequest,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    if !request.only_cache {
        bail!("reset requires --only-cache; no other reset operation is available in v1");
    }

    let workspace = workspace::discover_from(cwd)?;
    let reset = cache::reset_cache(&workspace)?;

    if output_options.json {
        return Ok(CommandOutput::success(render_reset_json(&reset)?));
    }

    Ok(CommandOutput::success(render_reset(&reset)))
}

fn render_reset(reset: &cache::CacheReset) -> String {
    if reset.removed {
        format!("Removed cache directory {}\n", reset.cache_dir.display())
    } else {
        format!(
            "Cache directory {} did not exist\n",
            reset.cache_dir.display()
        )
    }
}

fn render_reset_json(reset: &cache::CacheReset) -> Result<String> {
    let output = ResetJson {
        cache_dir: reset.cache_dir.display().to_string(),
        removed: reset.removed,
    };
    let mut json =
        serde_json::to_string_pretty(&output).context("failed to serialize reset JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct ResetJson {
    cache_dir: String,
    removed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn reset_only_cache_removes_configured_cache_dir() {
        let test_workspace = TestWorkspace::new("gomo-reset-command-test");
        test_workspace.write_file(
            "gomo.toml",
            r#"
[cache]
dir = "tmp/cache"
"#,
        );
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );
        test_workspace.write_file("tmp/cache/v1/task/entry", "cached\n");

        let output = run(
            test_workspace.path(),
            ResetRequest { only_cache: true },
            OutputOptions::default(),
        )
        .expect("cache reset should succeed");

        assert!(output.stdout.contains("Removed cache directory"));
        assert!(!test_workspace.path().join("tmp/cache").exists());
    }

    #[test]
    fn reset_requires_only_cache() {
        let test_workspace = TestWorkspace::new("gomo-reset-command-test");
        test_workspace.write_gomo_config();

        let error = run(
            test_workspace.path(),
            ResetRequest { only_cache: false },
            OutputOptions::default(),
        )
        .expect_err("reset without --only-cache should fail");

        assert!(error.to_string().contains("reset requires --only-cache"));
    }

    #[test]
    fn reset_renders_json() {
        let test_workspace = TestWorkspace::new("gomo-reset-command-test");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "libs/shared",
            r#"
name = "shared"
version = "0.1.0"
"#,
        );

        let output = run(
            test_workspace.path(),
            ResetRequest { only_cache: true },
            OutputOptions {
                json: true,
                ci: true,
                tui: false,
                terminal_width: None,
            },
        )
        .expect("JSON reset should succeed");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert_eq!(value["removed"], false);
        assert!(
            value["cache_dir"]
                .as_str()
                .unwrap()
                .ends_with(".gomo/cache")
        );
    }
}
