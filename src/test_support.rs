use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) struct TestWorkspace {
    root: PathBuf,
}

impl TestWorkspace {
    pub(crate) fn new(prefix: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), unique_id()));
        fs::create_dir_all(&root).expect("test workspace should be created");
        Self { root }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn write_gomo_config(&self) {
        self.write_file(
            "gomo.toml",
            r#"
[workspace]
project_roots = ["apps/*", "libs/*", "services/*"]
"#,
        );
    }

    pub(crate) fn write_manifest(&self, project_path: &str, contents: &str) -> PathBuf {
        self.write_file(&format!("{project_path}/gleam.toml"), contents)
    }

    pub(crate) fn write_file(&self, relative_path: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        let parent = path.parent().expect("test file should have a parent");
        fs::create_dir_all(parent).expect("test directory should be created");
        fs::write(&path, contents).expect("test file should be written");
        path
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn unique_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}
