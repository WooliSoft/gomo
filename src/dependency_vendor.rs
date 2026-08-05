use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::header::AUTHORIZATION;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};
use walkdir::WalkDir;

use crate::gleam_lock::{LockedPackage, LockedPackageSource, parse_lock_manifest};
use crate::gleam_toml::parse_manifest;
use crate::workspace::{Project, Workspace};

const INVENTORY_FILE: &str = "gomo-vendor.toml";
const INVENTORY_SCHEMA: u32 = 1;
const HEX_TARBALL_BASE_URL: &str = "https://repo.hex.pm/tarballs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VendorInventory {
    schema: u32,
    packages: Vec<VendorPackage>,
    projects: Vec<VendorProject>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct VendorPackage {
    name: String,
    version: String,
    source: VendorPackageSource,
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    outer_checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_checksum: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum VendorPackageSource {
    Hex,
    Git,
}

impl VendorPackageSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hex => "hex",
            Self::Git => "git",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VendorProject {
    name: String,
    path: String,
    gleam_toml_checksum: String,
    manifest_toml_checksum: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    path_dependencies: Vec<VendorPathDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct VendorPathDependency {
    name: String,
    path: String,
    gleam_toml_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VendorCheckReport {
    pub(crate) status: String,
    pub(crate) directory: String,
    pub(crate) expected_package_count: usize,
    pub(crate) checked_package_count: usize,
    pub(crate) missing_artifacts: Vec<String>,
    pub(crate) invalid_artifacts: Vec<String>,
    pub(crate) stale_projects: Vec<String>,
    pub(crate) inventory_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VendorSyncReport {
    pub(crate) status: String,
    pub(crate) directory: String,
    pub(crate) package_count: usize,
    pub(crate) downloaded_count: usize,
    pub(crate) reused_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct GleamPackagesState {
    #[serde(default)]
    packages: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    git: BTreeMap<String, GleamGitState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GleamGitState {
    commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

pub(crate) fn sync(workspace: &Workspace) -> Result<VendorSyncReport> {
    let config = workspace
        .vendoring
        .as_ref()
        .context("dependency vendoring is not configured; add `[vendoring]` to gomo.toml")?;
    let (mut packages, projects) = expected_inventory(workspace)?;
    let previous = read_inventory(&config.dir).ok();
    let previous_packages = previous
        .map(|inventory| {
            inventory
                .packages
                .into_iter()
                .map(|package| (package_identity(&package), package))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    fs::create_dir_all(config.dir.join("hex"))
        .with_context(|| format!("failed to create vendor directory {}", config.dir.display()))?;
    fs::create_dir_all(config.dir.join("git"))
        .with_context(|| format!("failed to create vendor directory {}", config.dir.display()))?;

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .redirect(Policy::none())
        .build()
        .context("failed to configure the Hex package client")?;
    let api_key = std::env::var("HEXPM_READ_API_KEY").ok();
    let mut downloaded_count = 0;
    let mut reused_count = 0;

    for package in &mut packages {
        let destination = config.dir.join(&package.file);
        match package.source {
            VendorPackageSource::Hex => {
                let checksum = package
                    .outer_checksum
                    .as_deref()
                    .context("Hex package is missing outer_checksum")?;
                if destination.is_file() && sha256_file(&destination)? == checksum {
                    reused_count += 1;
                } else {
                    download_hex_package(&client, api_key.as_deref(), package, &destination)?;
                    downloaded_count += 1;
                }
            }
            VendorPackageSource::Git => {
                let previous = previous_packages.get(&package_identity(package));
                let reusable_checksum =
                    previous.and_then(|package| package.archive_checksum.as_ref());
                if let Some(checksum) = reusable_checksum
                    && destination.is_file()
                    && sha256_file(&destination)? == *checksum
                {
                    package.archive_checksum = Some(checksum.clone());
                    reused_count += 1;
                } else {
                    package.archive_checksum = Some(vendor_git_package(package, &destination)?);
                    downloaded_count += 1;
                }
            }
        }
    }

    packages.sort();
    let inventory = VendorInventory {
        schema: INVENTORY_SCHEMA,
        packages,
        projects,
    };
    write_inventory(&config.dir, &inventory)?;
    prune_stale_artifacts(&config.dir, &inventory.packages)?;

    Ok(VendorSyncReport {
        status: "ok".to_string(),
        directory: workspace_relative(workspace, &config.dir),
        package_count: inventory.packages.len(),
        downloaded_count,
        reused_count,
    })
}

pub(crate) fn check_workspace(workspace: &Workspace) -> Option<VendorCheckReport> {
    let config = workspace.vendoring.as_ref()?;
    let mut report = VendorCheckReport {
        status: "ok".to_string(),
        directory: workspace_relative(workspace, &config.dir),
        expected_package_count: 0,
        checked_package_count: 0,
        missing_artifacts: Vec::new(),
        invalid_artifacts: Vec::new(),
        stale_projects: Vec::new(),
        inventory_errors: Vec::new(),
    };
    let (expected_packages, expected_projects) = match expected_inventory(workspace) {
        Ok(expected) => expected,
        Err(error) => {
            report.inventory_errors.push(error.to_string());
            report.finish();
            return Some(report);
        }
    };
    report.expected_package_count = expected_packages.len();
    let inventory = match read_inventory(&config.dir) {
        Ok(inventory) => inventory,
        Err(error) => {
            report.inventory_errors.push(error.to_string());
            report.finish();
            return Some(report);
        }
    };
    if inventory.schema != INVENTORY_SCHEMA {
        report.inventory_errors.push(format!(
            "unsupported vendor inventory schema {}; expected {}",
            inventory.schema, INVENTORY_SCHEMA
        ));
        report.finish();
        return Some(report);
    }

    let inventory_projects = inventory
        .projects
        .iter()
        .map(|project| (project.path.as_str(), project))
        .collect::<BTreeMap<_, _>>();
    for project in &expected_projects {
        if inventory_projects.get(project.path.as_str()).copied() != Some(project) {
            report.stale_projects.push(project.path.clone());
        }
    }

    let inventory_packages = inventory
        .packages
        .iter()
        .map(|package| (package_identity(package), package))
        .collect::<BTreeMap<_, _>>();
    for expected in &expected_packages {
        let identity = package_identity(expected);
        let Some(actual) = inventory_packages.get(&identity).copied() else {
            report.missing_artifacts.push(package_display(expected));
            continue;
        };
        if !same_package_metadata(expected, actual) {
            report.invalid_artifacts.push(format!(
                "{} has stale inventory metadata",
                package_display(expected)
            ));
            continue;
        }
        let path = match vendor_artifact_path(&config.dir, actual) {
            Ok(path) => path,
            Err(error) => {
                report.invalid_artifacts.push(format!(
                    "{} has an invalid archive path: {error}",
                    package_display(expected)
                ));
                continue;
            }
        };
        if !path.is_file() {
            report.missing_artifacts.push(format!(
                "{} ({})",
                package_display(expected),
                actual.file
            ));
            continue;
        }
        let expected_checksum = match actual.source {
            VendorPackageSource::Hex => actual.outer_checksum.as_deref(),
            VendorPackageSource::Git => actual.archive_checksum.as_deref(),
        };
        match expected_checksum {
            Some(checksum) => match sha256_file(&path) {
                Ok(actual_checksum) if actual_checksum == checksum => {
                    report.checked_package_count += 1;
                }
                Ok(_) => report.invalid_artifacts.push(format!(
                    "{} has an invalid archive checksum",
                    package_display(expected)
                )),
                Err(error) => report.invalid_artifacts.push(format!(
                    "{} could not be checked: {error}",
                    package_display(expected)
                )),
            },
            None => report.invalid_artifacts.push(format!(
                "{} is missing an archive checksum",
                package_display(expected)
            )),
        }
    }
    report.finish();
    Some(report)
}

pub(crate) fn prepare_projects(workspace: &Workspace, project_names: &[String]) -> Result<()> {
    if workspace.vendoring.is_none() {
        return Ok(());
    }
    let hex_package_cache = dirs::cache_dir()
        .context("failed to determine the user cache directory")?
        .join("gleam/hex/hexpm/packages");
    prepare_projects_with_hex_cache(workspace, project_names, &hex_package_cache)
}

fn prepare_projects_with_hex_cache(
    workspace: &Workspace,
    project_names: &[String],
    hex_package_cache: &Path,
) -> Result<()> {
    let Some(config) = workspace.vendoring.as_ref() else {
        return Ok(());
    };
    let Some(report) = check_workspace(workspace) else {
        return Ok(());
    };
    if !report.is_success() {
        bail!(
            "vendored dependencies are incomplete or stale ({} issue(s)); run `gomo deps check`, then `gomo deps vendor`",
            report.issue_count()
        );
    }
    let inventory = read_inventory(&config.dir)?;
    hydrate_hex_cache(config.dir.as_path(), &inventory.packages, hex_package_cache)?;
    let packages = inventory
        .packages
        .iter()
        .map(|package| (package_identity(package), package))
        .collect::<BTreeMap<_, _>>();
    let selected = project_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for project in &workspace.projects {
        if selected.contains(project.name.as_str()) {
            prepare_project(config.dir.as_path(), project, &packages)?;
        }
    }
    Ok(())
}

impl VendorCheckReport {
    pub(crate) fn is_success(&self) -> bool {
        self.status == "ok"
    }

    pub(crate) fn issue_count(&self) -> usize {
        self.missing_artifacts.len()
            + self.invalid_artifacts.len()
            + self.stale_projects.len()
            + self.inventory_errors.len()
    }

    fn finish(&mut self) {
        self.missing_artifacts.sort();
        self.invalid_artifacts.sort();
        self.stale_projects.sort();
        self.inventory_errors.sort();
        if self.issue_count() > 0 {
            self.status = "error".to_string();
        }
    }
}

fn expected_inventory(workspace: &Workspace) -> Result<(Vec<VendorPackage>, Vec<VendorProject>)> {
    let mut packages = BTreeMap::<String, VendorPackage>::new();
    let mut projects = Vec::new();
    for project in &workspace.projects {
        let manifest_path = project.root.join("manifest.toml");
        if !manifest_path.is_file() {
            bail!(
                "project `{}` is missing {}",
                project.name,
                workspace_relative(workspace, &manifest_path)
            );
        }
        let manifest = parse_lock_manifest(&manifest_path)?;
        for package in manifest.packages {
            let Some(package) = expected_package(&package)? else {
                continue;
            };
            let identity = package_identity(&package);
            if let Some(previous) = packages.insert(identity, package.clone())
                && !same_package_metadata(&previous, &package)
            {
                bail!(
                    "vendored package identity collision between {} and {}",
                    package_display(&previous),
                    package_display(&package)
                );
            }
        }
        let mut path_dependencies = Vec::new();
        for dependency in &project.path_dependencies {
            let root = project
                .root
                .join(&dependency.path)
                .canonicalize()
                .with_context(|| {
                    format!(
                        "failed to resolve path dependency `{}` for `{}`",
                        dependency.name, project.name
                    )
                })?;
            let gleam_toml = root.join("gleam.toml");
            path_dependencies.push(VendorPathDependency {
                name: dependency.name.clone(),
                path: workspace_relative(workspace, &gleam_toml),
                gleam_toml_checksum: sha256_file(&gleam_toml)?,
            });
        }
        path_dependencies.sort();
        projects.push(VendorProject {
            name: project.name.clone(),
            path: portable_path(&project.root_relative_path),
            gleam_toml_checksum: sha256_file(&project.manifest_path)?,
            manifest_toml_checksum: sha256_file(&manifest_path)?,
            path_dependencies,
        });
    }
    projects.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((packages.into_values().collect(), projects))
}

fn expected_package(package: &LockedPackage) -> Result<Option<VendorPackage>> {
    match &package.source {
        LockedPackageSource::Local => Ok(None),
        LockedPackageSource::Hex => {
            let checksum = package.outer_checksum.as_deref().with_context(|| {
                format!(
                    "Hex package `{} {}` is missing outer_checksum",
                    package.name, package.version
                )
            })?;
            validate_sha256(checksum).with_context(|| {
                format!(
                    "Hex package `{} {}` has invalid outer_checksum",
                    package.name, package.version
                )
            })?;
            Ok(Some(VendorPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: VendorPackageSource::Hex,
                file: format!("hex/{checksum}.tar"),
                outer_checksum: Some(checksum.to_string()),
                repo: None,
                commit: None,
                path: None,
                archive_checksum: None,
            }))
        }
        LockedPackageSource::Git => {
            let repo = package.repo.clone().with_context(|| {
                format!(
                    "Git package `{} {}` is missing repo",
                    package.name, package.version
                )
            })?;
            let commit = package.commit.clone().with_context(|| {
                format!(
                    "Git package `{} {}` is missing commit",
                    package.name, package.version
                )
            })?;
            let path = package
                .path
                .as_ref()
                .map(|path| normalized_relative_path(path, "Git package path"))
                .transpose()?;
            let mut hasher = blake3::Hasher::new();
            for part in [&package.name, &package.version, &repo, &commit] {
                hasher.update(part.as_bytes());
                hasher.update(&[0]);
            }
            if let Some(path) = &path {
                hasher.update(path.as_bytes());
            }
            let key = hasher.finalize().to_hex();
            Ok(Some(VendorPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: VendorPackageSource::Git,
                file: format!("git/{key}.tar.zst"),
                outer_checksum: None,
                repo: Some(repo),
                commit: Some(commit),
                path,
                archive_checksum: None,
            }))
        }
        LockedPackageSource::Other(source) => bail!(
            "package `{} {}` uses unsupported source `{}`",
            package.name,
            package.version,
            source
        ),
    }
}

fn download_hex_package(
    client: &Client,
    api_key: Option<&str>,
    package: &VendorPackage,
    destination: &Path,
) -> Result<()> {
    let checksum = package
        .outer_checksum
        .as_deref()
        .context("Hex package is missing outer_checksum")?;
    let url = format!(
        "{HEX_TARBALL_BASE_URL}/{}-{}.tar",
        package.name, package.version
    );
    let mut request = client.get(&url);
    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, api_key);
    }
    let response = request
        .send()
        .with_context(|| format!("failed to download {}", package_display(package)))?;
    if response.status().is_redirection() {
        bail!(
            "refused redirect while downloading {}",
            package_display(package)
        );
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("failed to download {}", package_display(package)))?;
    let bytes = response
        .bytes()
        .with_context(|| format!("failed to read {} archive", package_display(package)))?;
    let actual = sha256_bytes(&bytes);
    if actual != checksum {
        bail!(
            "downloaded {} with checksum {actual}, expected {checksum}",
            package_display(package)
        );
    }
    write_atomic(destination, &bytes)
}

fn vendor_git_package(package: &VendorPackage, destination: &Path) -> Result<String> {
    let repo = package
        .repo
        .as_deref()
        .context("Git package is missing repo")?;
    let commit = package
        .commit
        .as_deref()
        .context("Git package is missing commit")?;
    let checkout = TempDir::new().context("failed to create temporary Git checkout")?;
    run_git(checkout.path(), &["init"])?;
    run_git(checkout.path(), &["remote", "add", "origin", repo])?;
    if run_git(
        checkout.path(),
        &["fetch", "--depth", "1", "origin", commit],
    )
    .is_err()
    {
        run_git(checkout.path(), &["fetch", "origin"])?;
    }
    run_git(checkout.path(), &["checkout", "--detach", commit])?;
    let resolved = run_git(checkout.path(), &["rev-parse", "HEAD"])?;
    if resolved.trim() != commit {
        bail!(
            "Git package {} resolved commit {}, expected {commit}",
            package_display(package),
            resolved.trim()
        );
    }
    let package_root = match &package.path {
        Some(path) => checkout.path().join(path),
        None => checkout.path().to_path_buf(),
    }
    .canonicalize()
    .with_context(|| {
        format!(
            "failed to resolve Git package path for {}",
            package_display(package)
        )
    })?;
    let checkout_root = checkout.path().canonicalize()?;
    if !package_root.starts_with(&checkout_root) {
        bail!(
            "Git package path for {} resolves outside its repository",
            package_display(package)
        );
    }
    let manifest_path = package_root.join("gleam.toml");
    let manifest = parse_manifest(&manifest_path).with_context(|| {
        format!(
            "vendored Git package {} does not contain a valid gleam.toml",
            package_display(package)
        )
    })?;
    if manifest.name != package.name || manifest.version.as_deref() != Some(&package.version) {
        bail!(
            "vendored Git package {} declares `{} {}`",
            package_display(package),
            manifest.name,
            manifest.version.as_deref().unwrap_or("<missing version>")
        );
    }
    write_git_archive(&package_root, destination)?;
    sha256_file(destination)
}

fn write_git_archive(root: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("vendor archive has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = NamedTempFile::new_in(parent).context("failed to create vendor archive")?;
    let output = temporary.reopen()?;
    let mut encoder = zstd::Encoder::new(output, 9)?;
    encoder.include_checksum(true)?;
    let mut archive = tar::Builder::new(encoder);
    let entries = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(path)?;
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, relative, io::empty())?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            validate_archive_link_target(&target)?;
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_link_name(target)?;
            header.set_cksum();
            archive.append_data(&mut header, relative, io::empty())?;
        } else if metadata.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(metadata.len());
            header.set_mode(file_mode(&metadata));
            header.set_cksum();
            archive.append_data(&mut header, relative, File::open(path)?)?;
        }
    }
    let encoder = archive.into_inner()?;
    let mut file = encoder.finish()?;
    file.flush()?;
    replace_file(temporary, destination)
}

fn hydrate_hex_cache(
    vendor_dir: &Path,
    packages: &[VendorPackage],
    hex_package_cache: &Path,
) -> Result<()> {
    // Verified against Gleam 1.18.0's internal checksum cache layout.
    fs::create_dir_all(hex_package_cache).with_context(|| {
        format!(
            "failed to create Gleam package cache {}",
            hex_package_cache.display()
        )
    })?;
    for package in packages
        .iter()
        .filter(|package| package.source == VendorPackageSource::Hex)
    {
        let checksum = package
            .outer_checksum
            .as_deref()
            .context("Hex package is missing outer_checksum")?;
        let source = vendor_artifact_path(vendor_dir, package)?;
        let destination = hex_package_cache.join(format!("{checksum}.tar"));
        if destination.is_file() && sha256_file(&destination)? == checksum {
            continue;
        }
        let bytes = fs::read(&source)
            .with_context(|| format!("failed to read vendored archive {}", source.display()))?;
        write_atomic(&destination, &bytes)?;
    }
    Ok(())
}

fn prepare_project(
    vendor_dir: &Path,
    project: &Project,
    inventory: &BTreeMap<String, &VendorPackage>,
) -> Result<()> {
    let manifest = parse_lock_manifest(&project.root.join("manifest.toml"))?;
    let packages_dir = project.root.join("build/packages");
    fs::create_dir_all(&packages_dir)?;
    let state_path = packages_dir.join("packages.toml");
    let old_state = read_gleam_packages_state(&state_path).unwrap_or_default();
    let mut state = GleamPackagesState::default();
    for package in &manifest.packages {
        match &package.source {
            LockedPackageSource::Hex => {
                let package_dir = packages_dir.join(&package.name);
                if package_dir.is_dir()
                    && old_state.packages.get(&package.name) == Some(&package.version)
                {
                    state
                        .packages
                        .insert(package.name.clone(), package.version.clone());
                } else if package_dir.exists() {
                    remove_path(&package_dir)?;
                }
            }
            LockedPackageSource::Local => {
                state
                    .packages
                    .insert(package.name.clone(), package.version.clone());
            }
            LockedPackageSource::Git => {
                let expected =
                    expected_package(package)?.context("Git package was not external")?;
                let vendored = inventory
                    .get(&package_identity(&expected))
                    .copied()
                    .with_context(|| {
                        format!(
                            "missing {} from vendor inventory",
                            package_display(&expected)
                        )
                    })?;
                let destination = packages_dir.join(&package.name);
                let expected_git = GleamGitState {
                    commit: package
                        .commit
                        .clone()
                        .context("Git package is missing commit")?,
                    path: expected.path.clone(),
                };
                let fresh = destination.is_dir()
                    && old_state.packages.get(&package.name) == Some(&package.version)
                    && old_state.git.get(&package.name) == Some(&expected_git);
                if !fresh {
                    extract_git_archive(
                        &vendor_artifact_path(vendor_dir, vendored)?,
                        &destination,
                    )?;
                }
                state
                    .packages
                    .insert(package.name.clone(), package.version.clone());
                state.git.insert(package.name.clone(), expected_git);
            }
            LockedPackageSource::Other(source) => bail!("unsupported package source `{source}`"),
        }
    }
    for dependency in &project.path_dependencies {
        let source = project.root.join(&dependency.path).join("gleam.toml");
        let text = fs::read_to_string(&source).with_context(|| {
            format!("failed to read path dependency config {}", source.display())
        })?;
        let fingerprint = xxhash_rust::xxh3::xxh3_64(text.as_bytes()).to_string();
        fs::write(
            packages_dir.join(format!("{}.config_fingerprint", dependency.name)),
            fingerprint,
        )?;
    }
    write_toml_atomic(&state_path, &state)
}

fn extract_git_archive(archive: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("Git package destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".gomo-vendor-")
        .tempdir_in(parent)?;
    let decoder = zstd::Decoder::new(File::open(archive)?)?;
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        normalized_relative_path(&path, "Git archive entry")?;
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::Symlink | tar::EntryType::Link
        ) {
            let target = entry
                .link_name()?
                .context("Git archive link is missing its target")?;
            validate_archive_link_target(&target)?;
        }
        if !entry.unpack_in(temporary.path())? {
            bail!(
                "Git archive entry `{}` escapes its destination",
                path.display()
            );
        }
    }
    let staged = temporary.keep();
    replace_directory(&staged, destination)
}

fn read_inventory(dir: &Path) -> Result<VendorInventory> {
    let path = dir.join(INVENTORY_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read vendor inventory {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid vendor inventory {}", path.display()))
}

fn write_inventory(dir: &Path, inventory: &VendorInventory) -> Result<()> {
    write_toml_atomic(&dir.join(INVENTORY_FILE), inventory)
}

fn read_gleam_packages_state(path: &Path) -> Result<GleamPackagesState> {
    let text = fs::read_to_string(path)?;
    toml::from_str(&text).context("invalid Gleam build/packages/packages.toml")
}

fn write_toml_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = toml::to_string_pretty(value).context("failed to serialize TOML")?;
    write_atomic(path, text.as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("output path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    replace_file(temporary, path)
}

fn replace_file(temporary: NamedTempFile, destination: &Path) -> Result<()> {
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", destination.display()))?;
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn same_package_metadata(expected: &VendorPackage, actual: &VendorPackage) -> bool {
    expected.name == actual.name
        && expected.version == actual.version
        && expected.source == actual.source
        && expected.file == actual.file
        && expected.outer_checksum == actual.outer_checksum
        && expected.repo == actual.repo
        && expected.commit == actual.commit
        && expected.path == actual.path
}

fn package_identity(package: &VendorPackage) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        package.source.as_str(),
        package.name,
        package.version,
        package.repo.as_deref().unwrap_or(""),
        package.commit.as_deref().unwrap_or(""),
        package.path.as_deref().unwrap_or("")
    )
}

fn package_display(package: &VendorPackage) -> String {
    format!(
        "{} {} ({})",
        package.name,
        package.version,
        package.source.as_str()
    )
}

fn normalized_relative_path(path: &Path, field: &str) -> Result<String> {
    if path.is_absolute() {
        bail!("{field} must be relative");
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            bail!("{field} must not escape its repository");
        }
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn validate_archive_link_target(target: &Path) -> Result<()> {
    normalized_relative_path(target, "Git archive link target").map(|_| ())
}

fn vendor_artifact_path(vendor_dir: &Path, package: &VendorPackage) -> Result<std::path::PathBuf> {
    let relative = Path::new(&package.file);
    normalized_relative_path(relative, "vendor archive path")?;
    let expected_parent = match package.source {
        VendorPackageSource::Hex => "hex",
        VendorPackageSource::Git => "git",
    };
    if !relative.starts_with(expected_parent) {
        bail!("archive must be stored under `{expected_parent}/`");
    }
    Ok(vendor_dir.join(relative))
}

fn replace_directory(staged: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("replacement directory has no parent")?;
    let backup = tempfile::Builder::new()
        .prefix(".gomo-vendor-backup-")
        .tempdir_in(parent)?
        .keep();
    fs::remove_dir(&backup)?;
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        if had_destination {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error).with_context(|| {
            format!(
                "failed to install vendored Git package at {}",
                destination.display()
            )
        });
    }
    if had_destination {
        remove_path(&backup)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn prune_stale_artifacts(vendor_dir: &Path, packages: &[VendorPackage]) -> Result<()> {
    let expected = packages
        .iter()
        .map(|package| package.file.as_str())
        .collect::<BTreeSet<_>>();
    for subdirectory in ["hex", "git"] {
        let directory = vendor_dir.join(subdirectory);
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let relative = format!("{subdirectory}/{}", entry.file_name().to_string_lossy());
            if !expected.contains(relative.as_str()) {
                remove_path(&entry.path())?;
            }
        }
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected a 64-character hexadecimal SHA-256 checksum");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:X}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:X}", Sha256::digest(bytes))
}

fn workspace_relative(workspace: &Workspace, path: &Path) -> String {
    portable_path(path.strip_prefix(&workspace.root).unwrap_or(path))
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o100 == 0 {
        0o644
    } else {
        0o755
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorkspace;

    #[test]
    fn checks_complete_hex_vendor_inventory() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let bytes = b"hex package";
        let checksum = sha256_bytes(bytes);
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest("apps/demo", "name = \"demo\"\nversion = \"0.1.0\"\n");
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            &format!(
                "packages = [\n  {{ name = \"gleam_stdlib\", version = \"1.0.0\", build_tools = [\"gleam\"], requirements = [], source = \"hex\", outer_checksum = \"{checksum}\" }},\n]\n"
            ),
        );
        let workspace = crate::workspace::discover(test_workspace.path()).unwrap();
        let (packages, projects) = expected_inventory(&workspace).unwrap();
        let inventory = VendorInventory {
            schema: INVENTORY_SCHEMA,
            packages,
            projects,
        };
        test_workspace.write_file(&format!("vendor/hex/{checksum}.tar"), "hex package");
        write_inventory(&test_workspace.path().join("vendor"), &inventory).unwrap();

        let report = check_workspace(&workspace).unwrap();

        assert!(report.is_success());
        assert_eq!(report.checked_package_count, 1);
    }

    #[test]
    fn hydrates_hex_packages_into_the_supplied_cache() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let bytes = b"hex package";
        let checksum = sha256_bytes(bytes);
        let vendor_dir = test_workspace.path().join("vendor");
        let cache_dir = test_workspace.path().join("gleam-cache");
        test_workspace.write_file(&format!("vendor/hex/{checksum}.tar"), "hex package");
        let package = VendorPackage {
            name: "gleam_stdlib".to_string(),
            version: "1.0.0".to_string(),
            source: VendorPackageSource::Hex,
            file: format!("hex/{checksum}.tar"),
            outer_checksum: Some(checksum.clone()),
            repo: None,
            commit: None,
            path: None,
            archive_checksum: None,
        };

        hydrate_hex_cache(&vendor_dir, &[package], &cache_dir).unwrap();

        assert_eq!(
            fs::read(cache_dir.join(format!("{checksum}.tar"))).unwrap(),
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalizes_archived_file_modes() {
        use std::os::unix::fs::PermissionsExt;

        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let path = test_workspace.write_file("source/file", "contents");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(file_mode(&fs::metadata(&path).unwrap()), 0o644);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o710)).unwrap();
        assert_eq!(file_mode(&fs::metadata(&path).unwrap()), 0o755);
    }

    #[test]
    fn reports_stale_project_and_corrupt_archive() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let checksum = sha256_bytes(b"expected");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest("apps/demo", "name = \"demo\"\nversion = \"0.1.0\"\n");
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            &format!(
                "packages = [\n  {{ name = \"gleam_stdlib\", version = \"1.0.0\", build_tools = [\"gleam\"], requirements = [], source = \"hex\", outer_checksum = \"{checksum}\" }},\n]\n"
            ),
        );
        let workspace = crate::workspace::discover(test_workspace.path()).unwrap();
        let (packages, mut projects) = expected_inventory(&workspace).unwrap();
        projects[0].gleam_toml_checksum = "stale".to_string();
        let inventory = VendorInventory {
            schema: INVENTORY_SCHEMA,
            packages,
            projects,
        };
        test_workspace.write_file(&format!("vendor/hex/{checksum}.tar"), "corrupt");
        write_inventory(&test_workspace.path().join("vendor"), &inventory).unwrap();

        let report = check_workspace(&workspace).unwrap();

        assert!(!report.is_success());
        assert_eq!(report.stale_projects, vec!["apps/demo"]);
        assert_eq!(report.invalid_artifacts.len(), 1);
    }

    #[test]
    fn gleam_packages_state_serializes_git_freshness() {
        let state = GleamPackagesState {
            packages: BTreeMap::from([("lustre".to_string(), "5.7.0".to_string())]),
            git: BTreeMap::from([(
                "lustre".to_string(),
                GleamGitState {
                    commit: "abc123".to_string(),
                    path: None,
                },
            )]),
        };

        let text = toml::to_string(&state).unwrap();

        assert!(text.contains("[packages]"));
        assert!(text.contains("[git.lustre]"));
        assert_eq!(toml::from_str::<GleamPackagesState>(&text).unwrap(), state);
    }

    #[test]
    fn prepares_git_packages_and_gleam_freshness_state() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest("apps/demo", "name = \"demo\"\nversion = \"0.1.0\"\n");
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            "packages = [\n  { name = \"example_git\", version = \"1.2.3\", build_tools = [\"gleam\"], requirements = [], source = \"git\", repo = \"https://invalid.example/repo.git\", commit = \"abc123\" },\n]\n",
        );
        test_workspace.write_file(
            "git-source/gleam.toml",
            "name = \"example_git\"\nversion = \"1.2.3\"\n",
        );
        test_workspace.write_file("git-source/src/example_git.gleam", "pub fn value() { 1 }\n");
        let workspace = crate::workspace::discover(test_workspace.path()).unwrap();
        let (mut packages, projects) = expected_inventory(&workspace).unwrap();
        let archive = test_workspace.path().join("vendor").join(&packages[0].file);
        write_git_archive(&test_workspace.path().join("git-source"), &archive).unwrap();
        packages[0].archive_checksum = Some(sha256_file(&archive).unwrap());
        write_inventory(
            &test_workspace.path().join("vendor"),
            &VendorInventory {
                schema: INVENTORY_SCHEMA,
                packages,
                projects,
            },
        )
        .unwrap();

        prepare_projects_with_hex_cache(
            &workspace,
            &["demo".to_string()],
            &test_workspace.path().join("gleam-cache"),
        )
        .unwrap();

        assert!(
            test_workspace
                .path()
                .join("apps/demo/build/packages/example_git/src/example_git.gleam")
                .is_file()
        );
        let state = read_gleam_packages_state(
            &test_workspace
                .path()
                .join("apps/demo/build/packages/packages.toml"),
        )
        .unwrap();
        assert_eq!(
            state.packages.get("example_git").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(
            state
                .git
                .get("example_git")
                .map(|state| state.commit.as_str()),
            Some("abc123")
        );
    }

    #[test]
    fn removes_stale_hex_source_before_marking_packages_fresh() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let checksum = sha256_bytes(b"new hex archive");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest("apps/demo", "name = \"demo\"\nversion = \"0.1.0\"\n");
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            &format!(
                "packages = [\n  {{ name = \"gleam_stdlib\", version = \"2.0.0\", build_tools = [\"gleam\"], requirements = [], source = \"hex\", outer_checksum = \"{checksum}\" }},\n]\n"
            ),
        );
        test_workspace.write_file(
            "apps/demo/build/packages/gleam_stdlib/gleam.toml",
            "name = \"gleam_stdlib\"\nversion = \"1.0.0\"\n",
        );
        write_toml_atomic(
            &test_workspace
                .path()
                .join("apps/demo/build/packages/packages.toml"),
            &GleamPackagesState {
                packages: BTreeMap::from([("gleam_stdlib".to_string(), "1.0.0".to_string())]),
                git: BTreeMap::new(),
            },
        )
        .unwrap();
        let workspace = crate::workspace::discover(test_workspace.path()).unwrap();
        let project = &workspace.projects[0];

        prepare_project(
            &test_workspace.path().join("vendor"),
            project,
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(
            !test_workspace
                .path()
                .join("apps/demo/build/packages/gleam_stdlib")
                .exists()
        );
        let state = read_gleam_packages_state(
            &test_workspace
                .path()
                .join("apps/demo/build/packages/packages.toml"),
        )
        .unwrap();
        assert!(!state.packages.contains_key("gleam_stdlib"));
    }

    #[test]
    fn rejects_unsafe_archive_paths_and_links() {
        let package = VendorPackage {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            source: VendorPackageSource::Hex,
            file: "../outside.tar".to_string(),
            outer_checksum: Some("A".repeat(64)),
            repo: None,
            commit: None,
            path: None,
            archive_checksum: None,
        };

        assert!(vendor_artifact_path(Path::new("vendor"), &package).is_err());
        assert!(validate_archive_link_target(Path::new("../../outside")).is_err());
        assert!(validate_archive_link_target(Path::new("/outside")).is_err());
        assert!(validate_archive_link_target(Path::new("inside/file")).is_ok());
    }

    #[test]
    fn atomic_file_write_replaces_existing_contents() {
        let test_workspace = TestWorkspace::new("gomo-vendor-test");
        let path = test_workspace.write_file("vendor/gomo-vendor.toml", "old");

        write_atomic(&path, b"new").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new");
    }

    #[test]
    fn prepared_git_package_builds_without_remote_access() {
        if Command::new("gleam").arg("--version").output().is_err() {
            eprintln!("skipping offline Gleam build test: `gleam` is not installed");
            return;
        }
        let test_workspace = TestWorkspace::new("gomo-vendor-offline-test");
        test_workspace.write_file(
            "gomo.toml",
            "[workspace]\nproject_roots = [\"apps/*\"]\n\n[vendoring]\n",
        );
        test_workspace.write_manifest(
            "apps/demo",
            "name = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nexample_git = { git = \"https://invalid.example/repo.git\", ref = \"abc123\" }\n",
        );
        test_workspace.write_file(
            "apps/demo/manifest.toml",
            "packages = [\n  { name = \"example_git\", version = \"1.2.3\", build_tools = [\"gleam\"], requirements = [], source = \"git\", repo = \"https://invalid.example/repo.git\", commit = \"abc123\" },\n]\n\n[requirements]\nexample_git = { git = \"https://invalid.example/repo.git\", ref = \"abc123\" }\n",
        );
        test_workspace.write_file(
            "apps/demo/src/demo.gleam",
            "import example_git\n\npub fn main() { example_git.value() }\n",
        );
        test_workspace.write_file(
            "git-source/gleam.toml",
            "name = \"example_git\"\nversion = \"1.2.3\"\n",
        );
        test_workspace.write_file("git-source/src/example_git.gleam", "pub fn value() { 1 }\n");
        let workspace = crate::workspace::discover(test_workspace.path()).unwrap();
        let (mut packages, projects) = expected_inventory(&workspace).unwrap();
        let archive = test_workspace.path().join("vendor").join(&packages[0].file);
        write_git_archive(&test_workspace.path().join("git-source"), &archive).unwrap();
        packages[0].archive_checksum = Some(sha256_file(&archive).unwrap());
        write_inventory(
            &test_workspace.path().join("vendor"),
            &VendorInventory {
                schema: INVENTORY_SCHEMA,
                packages,
                projects,
            },
        )
        .unwrap();
        prepare_projects_with_hex_cache(
            &workspace,
            &["demo".to_string()],
            &test_workspace.path().join("gleam-cache"),
        )
        .unwrap();

        let output = Command::new("gleam")
            .args(["build", "--no-print-progress"])
            .current_dir(test_workspace.path().join("apps/demo"))
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "offline Gleam build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
