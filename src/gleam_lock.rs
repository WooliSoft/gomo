use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Parsed package entries from a Gleam `manifest.toml` lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GleamLockManifest {
    pub(crate) packages: Vec<LockedPackage>,
}

/// A resolved package entry in `manifest.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockedPackage {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) source: LockedPackageSource,
    pub(crate) path: Option<PathBuf>,
}

/// Resolved package source from a Gleam `manifest.toml` package entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LockedPackageSource {
    Hex,
    Local,
    Other(String),
}

#[derive(Debug, Deserialize)]
struct RawLockManifest {
    #[serde(default)]
    packages: Vec<RawLockedPackage>,
}

#[derive(Debug, Deserialize)]
struct RawLockedPackage {
    name: String,
    version: String,
    source: String,
    path: Option<PathBuf>,
}

impl LockedPackageSource {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Hex => "hex",
            Self::Local => "local",
            Self::Other(source) => source.as_str(),
        }
    }

    fn parse(source: String) -> Self {
        match source.as_str() {
            "hex" => Self::Hex,
            "local" => Self::Local,
            _ => Self::Other(source),
        }
    }
}

impl fmt::Display for LockedPackageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse resolved packages from a Gleam `manifest.toml` lock file.
pub(crate) fn parse_lock_manifest(path: &Path) -> Result<GleamLockManifest> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read Gleam manifest lock {}", path.display()))?;
    let manifest = toml::from_str::<RawLockManifest>(&text)
        .with_context(|| format!("invalid TOML in {}", path.display()))?;

    let mut packages = Vec::new();
    for package in manifest.packages {
        let name = package.name.trim().to_string();
        let version = package.version.trim().to_string();
        let source = package.source.trim().to_string();
        if name.is_empty() {
            bail!("{} contains a package with an empty name", path.display());
        }
        if version.is_empty() {
            bail!(
                "{} contains package `{}` with an empty version",
                path.display(),
                name
            );
        }
        if source.is_empty() {
            bail!(
                "{} contains package `{}` with an empty source",
                path.display(),
                name
            );
        }

        packages.push(LockedPackage {
            name,
            version,
            source: LockedPackageSource::parse(source),
            path: package.path,
        });
    }

    packages.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.version.cmp(&right.version))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(GleamLockManifest { packages })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestWorkspace;

    #[test]
    fn parses_locked_package_versions() {
        let test_workspace = TestWorkspace::new("gomo-gleam-lock-test");
        let path = test_workspace.write_file(
            "manifest.toml",
            r#"
packages = [
  { name = "gleam_stdlib", version = "1.0.2", build_tools = ["gleam"], requirements = [], otp_app = "gleam_stdlib", source = "hex", outer_checksum = "abc" },
  { name = "shared", version = "0.1.0", build_tools = ["gleam"], requirements = ["gleam_stdlib"], source = "local", path = "../shared" },
]

[requirements]
gleam_stdlib = { version = ">= 1.0.0 and < 2.0.0" }
"#,
        );

        let manifest = parse_lock_manifest(&path).expect("lock manifest should parse");

        assert_eq!(manifest.packages.len(), 2);
        assert_eq!(manifest.packages[0].name, "gleam_stdlib");
        assert_eq!(manifest.packages[0].version, "1.0.2");
        assert_eq!(manifest.packages[0].source, LockedPackageSource::Hex);
        assert_eq!(manifest.packages[1].name, "shared");
        assert_eq!(manifest.packages[1].version, "0.1.0");
        assert_eq!(manifest.packages[1].source, LockedPackageSource::Local);
        assert_eq!(manifest.packages[1].path, Some(PathBuf::from("../shared")));
    }
}
