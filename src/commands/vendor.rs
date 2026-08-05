use std::path::Path;

use anyhow::{Context, Result};

use crate::commands::{CommandOutput, OutputOptions};
use crate::dependency_vendor::{self, VendorSyncReport};
use crate::workspace;

pub(crate) fn run(cwd: &Path, output_options: OutputOptions) -> Result<CommandOutput> {
    let workspace = workspace::discover_from(cwd)?;
    let report = dependency_vendor::sync(&workspace)?;
    let output = if output_options.json {
        render_json(&report)?
    } else {
        render_plain(&report)
    };
    Ok(CommandOutput::success(output))
}

fn render_json(report: &VendorSyncReport) -> Result<String> {
    let mut output =
        serde_json::to_string_pretty(report).context("failed to serialize vendor JSON")?;
    output.push('\n');
    Ok(output)
}

fn render_plain(report: &VendorSyncReport) -> String {
    format!(
        "Dependency Vendor\nStatus: {}\nDirectory: {}\nPackages: {}\nDownloaded: {}\nReused: {}\n[ok] vendor store is synchronized\n",
        report.status,
        report.directory,
        report.package_count,
        report.downloaded_count,
        report.reused_count
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn vendors_workspace_with_only_local_dependencies() {
        let test_workspace = TestWorkspace::new("gomo-vendor-command-test");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\", \"libs/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest(
            "apps/demo",
            "name = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nshared = { path = \"../../libs/shared\" }\n",
        );
        test_workspace.write_manifest("libs/shared", "name = \"shared\"\nversion = \"0.1.0\"\n");
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            "packages = [\n  { name = \"shared\", version = \"0.1.0\", build_tools = [\"gleam\"], requirements = [], source = \"local\", path = \"../../libs/shared\" },\n]\n",
        );
        test_workspace.write_file("libs/shared/manifest.toml", "packages = []\n");

        let output = run(
            test_workspace.path(),
            OutputOptions {
                ci: true,
                ..OutputOptions::default()
            },
        )
        .unwrap();

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("Packages: 0"));
        assert!(
            test_workspace
                .path()
                .join("vendor/gomo-vendor.toml")
                .is_file()
        );
    }
}
