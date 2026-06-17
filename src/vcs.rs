use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use git2::{Delta, DiffOptions, Repository};
use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::{ReadonlyRepo, Repo, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::revset::{RevsetExtensions, SymbolResolver};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{self as jj_workspace, Workspace as JjWorkspace};
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::workspace::Workspace;

/// Boundary for changed-file discovery.
pub(crate) trait ChangedFileSource {
    fn changed_files(&self, workspace: &Workspace) -> Result<Vec<PathBuf>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplicitChangedFiles {
    files: Vec<PathBuf>,
}

impl ExplicitChangedFiles {
    pub(crate) fn new(files: Vec<PathBuf>) -> Result<Self> {
        if files.is_empty() {
            bail!("affected requires --files with at least one changed file");
        }

        Ok(Self { files })
    }
}

impl ChangedFileSource for ExplicitChangedFiles {
    fn changed_files(&self, workspace: &Workspace) -> Result<Vec<PathBuf>> {
        normalize_changed_files(workspace, self.files.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VcsChangedFiles {
    base: String,
}

impl VcsChangedFiles {
    pub(crate) fn new(base: String) -> Self {
        Self { base }
    }
}

impl ChangedFileSource for VcsChangedFiles {
    fn changed_files(&self, workspace: &Workspace) -> Result<Vec<PathBuf>> {
        let base = self.base.trim();
        if base.is_empty() {
            bail!("affected --base must not be empty");
        }

        vcs_changed_files(workspace, base)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VcsKind {
    Jj,
    Git,
}

fn vcs_changed_files(workspace: &Workspace, base: &str) -> Result<Vec<PathBuf>> {
    match detect_vcs(&workspace.root)? {
        VcsKind::Jj => jj_changed_files(workspace, base),
        VcsKind::Git => git_changed_files(workspace, base),
    }
}

fn detect_vcs(root: &Path) -> Result<VcsKind> {
    if root.join(".jj").exists() {
        return Ok(VcsKind::Jj);
    }
    if root.join(".git").exists() {
        return Ok(VcsKind::Git);
    }

    bail!(
        "affected --base requires a Git or Jujutsu repository at {}",
        root.display()
    );
}

fn git_changed_files(workspace: &Workspace, base: &str) -> Result<Vec<PathBuf>> {
    let repo = Repository::open(&workspace.root).with_context(|| {
        format!(
            "failed to open Git repository at {}",
            workspace.root.display()
        )
    })?;
    let head_commit = repo
        .head()
        .context("failed to read Git HEAD")?
        .peel_to_commit()
        .context("Git HEAD does not point to a commit")?;
    let base_commit = repo
        .revparse_single(base)
        .with_context(|| format!("failed to resolve Git base `{base}`"))?
        .peel_to_commit()
        .with_context(|| format!("Git base `{base}` does not point to a commit"))?;
    let merge_base = repo
        .merge_base(base_commit.id(), head_commit.id())
        .with_context(|| format!("failed to find Git merge base for `{base}`"))?;
    let merge_base_tree = repo
        .find_commit(merge_base)
        .context("failed to load Git merge-base commit")?
        .tree()
        .context("failed to load Git merge-base tree")?;

    let mut options = DiffOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&merge_base_tree), Some(&mut options))
        .with_context(|| format!("failed to list Git changes from `{base}`"))?;
    let mut files = Vec::new();
    diff.foreach(
        &mut |delta, _progress| {
            let file = if delta.status() == Delta::Deleted {
                delta.old_file()
            } else {
                delta.new_file()
            };
            if let Some(path) = file.path() {
                files.push(path.to_path_buf());
            }
            true
        },
        None,
        None,
        None,
    )
    .context("failed to read Git diff entries")?;

    normalize_changed_files(workspace, files.iter())
}

fn jj_changed_files(workspace: &Workspace, base: &str) -> Result<Vec<PathBuf>> {
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .build()
        .context("failed to create Tokio runtime for Jujutsu repository access")?;
    runtime.block_on(jj_changed_files_async(workspace, base))
}

async fn jj_changed_files_async(workspace: &Workspace, base: &str) -> Result<Vec<PathBuf>> {
    let settings = UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
        .context("failed to create Jujutsu settings")?;
    let mut jj_workspace = JjWorkspace::load(
        &settings,
        &workspace.root,
        &StoreFactories::default(),
        &jj_workspace::default_working_copy_factories(),
    )
    .with_context(|| {
        format!(
            "failed to open Jujutsu repository at {}",
            workspace.root.display()
        )
    })?;
    let repo = jj_workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load Jujutsu repository")?;
    let base_tree = resolve_jj_single_commit(repo.as_ref(), jj_workspace.workspace_name(), base)
        .await?
        .tree();
    let snapshot_tree = snapshot_jj_working_copy(workspace, &mut jj_workspace)
        .await
        .context("failed to snapshot Jujutsu working copy")?;

    let mut files = Vec::new();
    let mut diff_stream = base_tree.diff_stream(&snapshot_tree, &EverythingMatcher);
    while let Some(entry) = diff_stream.next().await {
        entry.values.context("failed to read Jujutsu tree diff")?;
        files.push(PathBuf::from(entry.path.as_internal_file_string()));
    }

    normalize_changed_files(workspace, files.iter())
}

async fn resolve_jj_single_commit(
    repo: &ReadonlyRepo,
    workspace_name: &jj_lib::ref_name::WorkspaceName,
    revision: &str,
) -> Result<Commit> {
    let commit_id = if revision == "@" {
        repo.view()
            .get_wc_commit_id(workspace_name)
            .cloned()
            .context("Jujutsu workspace has no working-copy commit")?
    } else {
        let extensions = RevsetExtensions::default();
        let resolver = SymbolResolver::new(repo, extensions.symbol_resolvers());
        resolver
            .resolve_symbol(repo, revision)
            .with_context(|| format!("failed to resolve Jujutsu base `{revision}`"))?
    };

    repo.store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load Jujutsu commit `{revision}`"))
}

async fn snapshot_jj_working_copy(
    workspace: &Workspace,
    jj_workspace: &mut JjWorkspace,
) -> Result<MergedTree> {
    let mut locked_workspace = jj_workspace
        .start_working_copy_mutation()
        .await
        .context("failed to lock Jujutsu working copy")?;
    let base_ignores = jj_base_ignores(workspace)?;
    let (tree, _stats) = locked_workspace
        .locked_wc()
        .snapshot(&SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: u64::MAX,
        })
        .await?;

    Ok(tree)
}

fn jj_base_ignores(workspace: &Workspace) -> Result<std::sync::Arc<GitIgnoreFile>> {
    GitIgnoreFile::empty()
        .chain_with_file(RepoPath::root(), workspace.root.join(".gitignore"))
        .with_context(|| {
            format!(
                "failed to load ignore patterns from {}",
                workspace.root.display()
            )
        })
}

fn normalize_changed_files<'a>(
    workspace: &Workspace,
    files: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut changed_files = BTreeSet::new();

    for file in files {
        let changed_file = normalize_changed_file(workspace, file)
            .with_context(|| format!("invalid changed file `{}`", file.display()))?;
        changed_files.insert(changed_file);
    }

    Ok(changed_files.into_iter().collect())
}

fn normalize_changed_file(workspace: &Workspace, file: &Path) -> Result<PathBuf> {
    if file.as_os_str().is_empty() {
        bail!("changed file path must not be empty");
    }

    let absolute_path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        workspace.root.join(file)
    };
    let normalized_path = normalize_lexically(&absolute_path);

    if !normalized_path.starts_with(&workspace.root) {
        bail!(
            "changed file must be inside workspace {}",
            workspace.root.display()
        );
    }

    let relative_path = normalized_path
        .strip_prefix(&workspace.root)
        .context("failed to make changed file workspace-relative")?;
    if relative_path.as_os_str().is_empty() {
        bail!("changed file must point inside the workspace root");
    }

    Ok(relative_path.to_path_buf())
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(component) => normalized.push(component),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;
    use crate::test_support::TestWorkspace;
    use crate::workspace;

    #[test]
    fn explicit_changed_files_are_normalized_relative_to_workspace() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        test_workspace.write_gomo_config();
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let absolute_file = workspace.root.join("apps/demo/src/main.gleam");
        let source = ExplicitChangedFiles::new(vec![
            PathBuf::from("apps/demo/src/main.gleam"),
            absolute_file,
        ])
        .expect("explicit files should be accepted");

        let changed_files = source
            .changed_files(&workspace)
            .expect("changed files should normalize");

        assert_eq!(changed_files, [PathBuf::from("apps/demo/src/main.gleam")]);
    }

    #[test]
    fn explicit_changed_files_reject_outside_paths() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        test_workspace.write_gomo_config();
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");
        let source = ExplicitChangedFiles::new(vec![PathBuf::from("../outside.gleam")])
            .expect("construction should accept raw paths");

        let error = source
            .changed_files(&workspace)
            .expect_err("outside path should fail");

        assert!(error.to_string().contains("invalid changed file"));
    }

    #[test]
    fn explicit_changed_files_require_at_least_one_file() {
        let error = ExplicitChangedFiles::new(Vec::new()).expect_err("empty files should fail");

        assert!(error.to_string().contains("affected requires --files"));
    }

    #[test]
    fn detect_vcs_prefers_jj_over_git() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        fs::create_dir_all(test_workspace.path().join(".git")).expect(".git should be created");
        fs::create_dir_all(test_workspace.path().join(".jj")).expect(".jj should be created");

        let vcs = detect_vcs(test_workspace.path()).expect("vcs should be detected");

        assert_eq!(vcs, VcsKind::Jj);
    }

    #[test]
    fn detect_vcs_uses_git_when_jj_is_absent() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        fs::create_dir_all(test_workspace.path().join(".git")).expect(".git should be created");

        let vcs = detect_vcs(test_workspace.path()).expect("vcs should be detected");

        assert_eq!(vcs, VcsKind::Git);
    }

    #[test]
    fn git_changed_files_include_diff_and_untracked_files() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        test_workspace.write_gomo_config();
        test_workspace.write_file(".gitignore", "*.log\n");
        test_workspace.write_file("README.md", "# demo\n");
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 1 }\n");
        let repo = Repository::init(test_workspace.path()).expect("Git repo should be created");
        commit_all(&repo, "initial");

        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 2 }\n");
        test_workspace.write_file("apps/demo/src/main.gleam", "pub fn main() { 1 }\n");
        test_workspace.write_file("ignored.log", "ignored\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");

        let changed_files =
            git_changed_files(&workspace, "HEAD").expect("Git changes should be listed");

        assert_eq!(
            changed_files,
            [
                PathBuf::from("apps/demo/src/main.gleam"),
                PathBuf::from("libs/shared/src/main.gleam"),
            ]
        );
    }

    #[test]
    fn jj_changed_files_with_base_at_compares_existing_wc_commit_to_snapshot() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        run_jj(test_workspace.path(), &["git", "init", "--no-colocate"]);
        test_workspace.write_gomo_config();
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 1 }\n");
        run_jj(test_workspace.path(), &["status"]);
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 2 }\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");

        let changed_files = jj_changed_files(&workspace, "@").expect("Jujutsu changes should list");

        assert_eq!(changed_files, [PathBuf::from("libs/shared/src/main.gleam")]);
    }

    #[test]
    fn jj_changed_files_do_not_include_ignored_untracked_files() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        run_jj(test_workspace.path(), &["git", "init", "--no-colocate"]);
        test_workspace.write_gomo_config();
        test_workspace.write_file(".gitignore", "ignored.log\nbuild/\n");
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 1 }\n");
        run_jj(test_workspace.path(), &["status"]);
        test_workspace.write_file("libs/shared/src/main.gleam", "pub fn main() { 2 }\n");
        test_workspace.write_file("ignored.log", "ignored\n");
        test_workspace.write_file("build/generated.txt", "generated\n");
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");

        let changed_files = jj_changed_files(&workspace, "@").expect("Jujutsu changes should list");

        assert_eq!(changed_files, [PathBuf::from("libs/shared/src/main.gleam")]);
    }

    #[test]
    fn vcs_changed_files_rejects_missing_vcs_metadata() {
        let test_workspace = TestWorkspace::new("gomo-vcs-test");
        test_workspace.write_gomo_config();
        let workspace = workspace::discover(test_workspace.path()).expect("workspace should load");

        let error = vcs_changed_files(&workspace, "main").expect_err("missing VCS should fail");

        assert!(
            error
                .to_string()
                .contains("requires a Git or Jujutsu repository")
        );
    }

    fn run_jj(repo: &Path, args: &[&str]) {
        let output = Command::new("jj")
            .arg("--no-pager")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("jj command should run");

        assert!(
            output.status.success(),
            "jj {} failed with stdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_all(repo: &Repository, message: &str) -> git2::Oid {
        let mut index = repo.index().expect("Git index should load");
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("Git index should add all files");
        index.write().expect("Git index should write");
        let tree_id = index.write_tree().expect("Git tree should write");
        let tree = repo.find_tree(tree_id).expect("Git tree should load");
        let signature = git2::Signature::now("Gomo Test", "gomo@example.com")
            .expect("Git signature should be created");
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let parents = parent.iter().collect::<Vec<_>>();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .expect("Git commit should be created")
    }
}
