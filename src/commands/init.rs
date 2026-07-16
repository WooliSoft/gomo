use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::commands::{CommandOutput, OutputOptions};

const PACKAGE_DIRS: &[&str] = &["apps/web", "libs/shared", "services/api"];

const TEMPLATE_FILES: &[TemplateFile] = &[
    TemplateFile::new("gomo.toml", include_str!("init/templates/gomo.toml")),
    TemplateFile::new("README.md", include_str!("init/templates/README.md")),
    TemplateFile::new(".gitignore", include_str!("init/templates/gitignore")),
    TemplateFile::new(
        "apps/web/gleam.toml",
        include_str!("init/templates/apps/web/gleam.toml"),
    ),
    TemplateFile::new(
        "apps/web/manifest.toml",
        include_str!("init/templates/apps/web/manifest.toml"),
    ),
    TemplateFile::new(
        "apps/web/.gitignore",
        include_str!("init/templates/apps/web/.gitignore"),
    ),
    TemplateFile::new(
        "apps/web/assets/styles.css",
        include_str!("init/templates/apps/web/assets/styles.css"),
    ),
    TemplateFile::new(
        "apps/web/src/web.gleam",
        include_str!("init/templates/apps/web/src/web.gleam"),
    ),
    TemplateFile::new(
        "apps/web/src/web/app.gleam",
        include_str!("init/templates/apps/web/src/web/app.gleam"),
    ),
    TemplateFile::new(
        "apps/web/test/web_test.gleam",
        include_str!("init/templates/apps/web/test/web_test.gleam"),
    ),
    TemplateFile::new(
        "libs/shared/gleam.toml",
        include_str!("init/templates/libs/shared/gleam.toml"),
    ),
    TemplateFile::new(
        "libs/shared/manifest.toml",
        include_str!("init/templates/libs/shared/manifest.toml"),
    ),
    TemplateFile::new(
        "libs/shared/.gitignore",
        include_str!("init/templates/libs/shared/.gitignore"),
    ),
    TemplateFile::new(
        "libs/shared/src/shared/api.gleam",
        include_str!("init/templates/libs/shared/src/shared/api.gleam"),
    ),
    TemplateFile::new(
        "libs/shared/test/shared_test.gleam",
        include_str!("init/templates/libs/shared/test/shared_test.gleam"),
    ),
    TemplateFile::new(
        "services/api/gleam.toml",
        include_str!("init/templates/services/api/gleam.toml"),
    ),
    TemplateFile::new(
        "services/api/manifest.toml",
        include_str!("init/templates/services/api/manifest.toml"),
    ),
    TemplateFile::new(
        "services/api/.gitignore",
        include_str!("init/templates/services/api/.gitignore"),
    ),
    TemplateFile::new(
        "services/api/src/api.gleam",
        include_str!("init/templates/services/api/src/api.gleam"),
    ),
    TemplateFile::new(
        "services/api/src/api/router.gleam",
        include_str!("init/templates/services/api/src/api/router.gleam"),
    ),
    TemplateFile::new(
        "services/api/test/api_test.gleam",
        include_str!("init/templates/services/api/test/api_test.gleam"),
    ),
];

const CI_TEMPLATE: &str = include_str!("init/templates/.github/workflows/ci.yml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitRequest {
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct TemplateFile {
    relative_path: &'static str,
    contents: &'static str,
}

impl TemplateFile {
    const fn new(relative_path: &'static str, contents: &'static str) -> Self {
        Self {
            relative_path,
            contents,
        }
    }
}

pub(crate) fn run(
    cwd: &Path,
    request: InitRequest,
    output_options: OutputOptions,
) -> Result<CommandOutput> {
    let target = if request.path.is_absolute() {
        request.path
    } else {
        cwd.join(request.path)
    };

    validate_target(&target)?;
    preflight(&target)?;
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create workspace directory {}", target.display()))?;

    let mut created_files = Vec::with_capacity(TEMPLATE_FILES.len() + 1);
    for template in TEMPLATE_FILES {
        write_template(&target, template.relative_path, template.contents)?;
        created_files.push(template.relative_path);
    }

    let ci = CI_TEMPLATE.replace("{{GOMO_VERSION}}", env!("CARGO_PKG_VERSION"));
    write_template(&target, ".github/workflows/ci.yml", &ci)?;
    created_files.push(".github/workflows/ci.yml");
    created_files.sort_unstable();

    let root = target.canonicalize().with_context(|| {
        format!(
            "failed to resolve initialized workspace {}",
            target.display()
        )
    })?;
    if output_options.json {
        return Ok(CommandOutput::success(render_json(&root, &created_files)?));
    }

    Ok(CommandOutput::success(format!(
        "Initialized Gomo workspace at {}\n\nSee README.md for development commands.\n",
        root.display()
    )))
}

fn validate_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to initialize through symlink {}",
                target.display()
            )
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("init target {} is not a directory", target.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect init target {}", target.display())),
    }
}

fn preflight(target: &Path) -> Result<()> {
    for package_dir in PACKAGE_DIRS {
        ensure_absent(&target.join(package_dir), "package directory")?;
    }
    for template in TEMPLATE_FILES {
        ensure_absent(&target.join(template.relative_path), "generated file")?;
    }
    ensure_absent(&target.join(".github/workflows/ci.yml"), "generated file")?;
    Ok(())
}

fn ensure_absent(path: &Path, kind: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite {kind} {}", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {kind} {}", path.display()))
        }
    }
}

fn write_template(target: &Path, relative_path: &str, contents: &str) -> Result<()> {
    let path = target.join(relative_path);
    let parent = path
        .parent()
        .with_context(|| format!("generated path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to create generated file {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write generated file {}", path.display()))
}

fn render_json(root: &Path, created_files: &[&str]) -> Result<String> {
    let output = InitJson {
        root: root.display().to_string(),
        files: created_files,
    };
    let mut json =
        serde_json::to_string_pretty(&output).context("failed to serialize init JSON")?;
    json.push('\n');
    Ok(json)
}

#[derive(Serialize)]
struct InitJson<'a> {
    root: String,
    files: &'a [&'a str],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::ProjectGraph;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    #[test]
    fn creates_full_stack_lustre_workspace() {
        let test_workspace = TestWorkspace::new("gomo-init-command-test");

        let output = run(
            test_workspace.path(),
            InitRequest {
                path: PathBuf::from("starter"),
            },
            OutputOptions::default(),
        )
        .expect("init should create the scaffold");
        let root = test_workspace.path().join("starter");

        assert!(output.stdout.contains("Initialized Gomo workspace"));
        assert!(root.join("gomo.toml").is_file());
        assert!(root.join("README.md").is_file());
        assert!(root.join(".github/workflows/ci.yml").is_file());
        assert!(root.join("apps/web/assets/styles.css").is_file());
        assert!(root.join("apps/web/manifest.toml").is_file());
        assert!(root.join("apps/web/.gitignore").is_file());
        assert!(root.join("apps/web/src/web/app.gleam").is_file());
        assert!(root.join("libs/shared/manifest.toml").is_file());
        assert!(root.join("libs/shared/.gitignore").is_file());
        assert!(root.join("libs/shared/src/shared/api.gleam").is_file());
        assert!(root.join("services/api/manifest.toml").is_file());
        assert!(root.join("services/api/.gitignore").is_file());
        assert!(root.join("services/api/src/api/router.gleam").is_file());
        assert!(!root.join("Justfile").exists());
        assert!(!root.join("devenv.nix").exists());
        assert!(!root.join("apps/web/index.html").exists());

        let web_config = fs::read_to_string(root.join("apps/web/gleam.toml"))
            .expect("web config should be readable");
        assert!(web_config.contains("target = \"javascript\""));
        assert!(web_config.contains("lustre_dev_tools"));
        assert!(web_config.contains("proxy = { from = \"/api\""));
        assert!(web_config.contains("../../libs/shared/src"));
        assert!(web_config.contains("../../services/api/priv/static"));

        let workflow = fs::read_to_string(root.join(".github/workflows/ci.yml"))
            .expect("workflow should be readable");
        assert!(workflow.contains("runs-on: ubuntu-latest"));
        assert!(workflow.contains("gomo --ci deps check"));
        assert!(workflow.contains(&format!(
            "gomo-v{}-x86_64-unknown-linux-gnu.tar.gz",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(workflow.contains("sha256sum --check -"));
        assert!(!workflow.contains("cargo install"));
        assert!(!workflow.contains("{{GOMO_VERSION}}"));
        assert!(!workflow.to_lowercase().contains("nix"));

        let workspace = workspace::discover(&root).expect("workspace should be discoverable");
        let names = workspace
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["web", "shared", "api"]);
        assert_eq!(workspace.project_globs, ["apps/*", "libs/*", "services/*"]);

        let graph = ProjectGraph::build(&workspace).expect("project graph should be valid");
        assert_eq!(graph.upstream["web"], ["shared"]);
        assert_eq!(graph.upstream["api"], ["shared"]);
        assert!(graph.upstream["shared"].is_empty());
    }

    #[test]
    fn initializes_existing_directory_without_touching_unrelated_files() {
        let test_workspace = TestWorkspace::new("gomo-init-command-test");
        test_workspace.write_file("notes.txt", "keep me\n");

        run(
            test_workspace.path(),
            InitRequest {
                path: PathBuf::from("."),
            },
            OutputOptions::default(),
        )
        .expect("init should use an existing directory");

        assert_eq!(
            fs::read_to_string(test_workspace.path().join("notes.txt"))
                .expect("unrelated file should remain readable"),
            "keep me\n"
        );
        assert!(test_workspace.path().join("gomo.toml").is_file());
    }

    #[test]
    fn refuses_collisions_before_writing_any_files() {
        let test_workspace = TestWorkspace::new("gomo-init-command-test");
        test_workspace.write_file("gomo.toml", "existing = true\n");

        let error = run(
            test_workspace.path(),
            InitRequest {
                path: PathBuf::from("."),
            },
            OutputOptions::default(),
        )
        .expect_err("init should refuse an existing managed file");

        assert!(error.to_string().contains("refusing to overwrite"));
        assert!(!test_workspace.path().join("apps").exists());
        assert_eq!(
            fs::read_to_string(test_workspace.path().join("gomo.toml"))
                .expect("existing config should remain readable"),
            "existing = true\n"
        );
    }

    #[test]
    fn refuses_to_merge_into_existing_package_directory() {
        let test_workspace = TestWorkspace::new("gomo-init-command-test");
        test_workspace.write_file("apps/web/notes.txt", "existing package\n");

        let error = run(
            test_workspace.path(),
            InitRequest {
                path: PathBuf::from("."),
            },
            OutputOptions::default(),
        )
        .expect_err("init should refuse an existing package directory");

        assert!(error.to_string().contains("package directory"));
        assert!(!test_workspace.path().join("gomo.toml").exists());
    }

    #[test]
    fn renders_created_files_as_json() {
        let test_workspace = TestWorkspace::new("gomo-init-command-test");

        let output = run(
            test_workspace.path(),
            InitRequest {
                path: PathBuf::from("starter"),
            },
            OutputOptions {
                json: true,
                ci: true,
                tui: false,
                terminal_width: None,
            },
        )
        .expect("JSON init should succeed");
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).expect("JSON should parse");

        assert!(value["root"].as_str().unwrap().ends_with("starter"));
        assert_eq!(value["files"].as_array().unwrap().len(), 22);
        assert!(
            value["files"]
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::String(
                    "apps/web/src/web.gleam".to_string()
                ))
        );
    }
}
