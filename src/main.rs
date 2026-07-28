mod affected;
mod cache;
mod cancellation;
mod cli;
mod commands;
mod completion;
mod dependency_versions;
mod gleam_lock;
mod gleam_toml;
mod graph;
mod remote_cache;
mod runner;
mod task;
#[cfg(test)]
mod test_support;
mod ui;
mod vcs;
mod workspace;

fn main() -> anyhow::Result<()> {
    cli::run()
}
