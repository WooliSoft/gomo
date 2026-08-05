use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use watchexec_events::{Event, FileType, Tag};

use crate::workspace::Workspace;

pub(crate) fn normalize_paths(workspace: &Workspace, events: &[Event]) -> (Vec<PathBuf>, bool) {
    let mut paths = BTreeSet::new();
    let mut structural = false;

    for event in events {
        if !is_actionable_watch_event(event) {
            continue;
        }
        for (path, file_type) in event.paths() {
            let relative = if path.is_absolute() {
                let Ok(relative) = path.strip_prefix(&workspace.root) else {
                    continue;
                };
                relative
            } else {
                path
            };
            let is_vendor_path = workspace
                .vendoring
                .as_ref()
                .and_then(|config| config.dir.strip_prefix(&workspace.root).ok())
                .is_some_and(|vendor| relative.starts_with(vendor));
            if relative.as_os_str().is_empty() || is_generated_path(relative) || is_vendor_path {
                continue;
            }
            let path_is_structural = is_structural_path(workspace, relative);
            let is_directory = file_type == Some(&FileType::Dir)
                || (file_type.is_none() && workspace.root.join(relative).is_dir());
            if should_include_watch_path(is_directory, path_is_structural) {
                structural |= path_is_structural;
                paths.insert(relative.to_path_buf());
            }
        }
    }

    (paths.into_iter().collect(), structural)
}

pub(crate) fn is_generated_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str();
        name == "build"
            || name == "target"
            || name == "node_modules"
            || name == ".gomo"
            || name.to_string_lossy().starts_with(".gomo-restore-")
            || name.to_string_lossy().starts_with(".gomo-backups-")
            || name == ".git"
            || name == ".jj"
            || name == ".devenv"
            || name == ".direnv"
    })
}

pub(crate) fn is_structural_path(workspace: &Workspace, path: &Path) -> bool {
    if path == Path::new("gomo.toml") || path.file_name().is_some_and(|name| name == "gleam.toml") {
        return true;
    }
    workspace.project_globs.iter().any(|glob| {
        glob.strip_suffix("/*").is_some_and(|parent| {
            let parent = Path::new(parent);
            path == parent
                || path
                    .strip_prefix(parent)
                    .is_ok_and(|relative| relative.components().count() <= 1)
        })
    })
}

fn is_actionable_watch_event(event: &Event) -> bool {
    !event.tags.iter().any(|tag| {
        matches!(
            tag,
            Tag::FileEventKind(kind) if kind.is_access()
        )
    })
}

fn should_include_watch_path(is_directory: bool, structural: bool) -> bool {
    !is_directory || structural
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;
    use crate::workspace;
    use watchexec_events::filekind::{AccessKind, FileEventKind};

    #[test]
    fn ignores_non_mutating_file_access_events() {
        let access = Event {
            tags: vec![Tag::FileEventKind(FileEventKind::Access(AccessKind::Read))],
            ..Event::default()
        };
        let change = Event {
            tags: vec![Tag::FileEventKind(FileEventKind::Any)],
            ..Event::default()
        };

        assert!(!is_actionable_watch_event(&access));
        assert!(is_actionable_watch_event(&change));
    }

    #[test]
    fn ignores_directory_registration_events_except_structural_paths() {
        assert!(!should_include_watch_path(true, false));
        assert!(should_include_watch_path(true, true));
        assert!(should_include_watch_path(false, false));
    }

    #[test]
    fn ignores_common_generated_directories() {
        assert!(is_generated_path(Path::new("tools/gomo/target/debug/gomo")));
        assert!(is_generated_path(Path::new(
            "apps/demo/node_modules/package/index.js"
        )));
        assert!(is_generated_path(Path::new(
            "libs/gohan_ui/.gomo-restore-2-123/build/output"
        )));
        assert!(!is_generated_path(Path::new(
            "libs/gohan_ui/src/gohan_ui.gleam"
        )));
    }

    #[test]
    fn normalizes_structural_paths_and_skips_paths_outside_the_workspace() {
        let test_workspace = TestWorkspace::new("gomo-watch-paths");
        test_workspace.write_gomo_config();
        test_workspace.write_manifest(
            "apps/demo",
            r#"
name = "demo"
version = "0.1.0"
"#,
        );
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let event = Event {
            tags: vec![
                Tag::Path {
                    path: workspace.root.join("apps/demo/gleam.toml"),
                    file_type: Some(FileType::File),
                },
                Tag::Path {
                    path: workspace.root.parent().unwrap().join("outside.gleam"),
                    file_type: Some(FileType::File),
                },
                Tag::Path {
                    path: workspace.root.join("node_modules/demo/gleam.toml"),
                    file_type: Some(FileType::File),
                },
            ],
            ..Event::default()
        };

        let (paths, structural) = normalize_paths(&workspace, &[event]);

        assert_eq!(paths, [PathBuf::from("apps/demo/gleam.toml")]);
        assert!(structural);
    }

    #[test]
    fn ignores_the_configured_vendor_directory() {
        let test_workspace = TestWorkspace::new("gomo-watch-paths");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\ndir = \"./third_party\"\n",
        );
        test_workspace.write_manifest("apps/demo", "name = \"demo\"\nversion = \"0.1.0\"\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let event = Event {
            tags: vec![Tag::Path {
                path: workspace.root.join("third_party/gomo-vendor.toml"),
                file_type: Some(FileType::File),
            }],
            ..Event::default()
        };

        let (paths, structural) = normalize_paths(&workspace, &[event]);

        assert!(paths.is_empty());
        assert!(!structural);
    }
}
