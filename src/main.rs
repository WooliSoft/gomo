mod affected;
mod cache;
mod cli;
mod commands;
mod dependency_versions;
mod gleam_lock;
mod gleam_toml;
mod graph;
mod runner;
#[cfg(test)]
mod test_support;
mod ui;
mod vcs;
mod workspace;

fn main() -> anyhow::Result<()> {
    cli::run()
}
