use std::env;

pub(crate) mod graph;
pub(crate) mod projects;
pub(crate) mod run;
mod terminal;

pub(crate) fn is_agent_environment() -> bool {
    env::var("AGENT").is_ok_and(|value| value == "1")
        || env::var("OPENCODE").is_ok_and(|value| value == "1")
        || env::var("CODEX_CI").is_ok_and(|value| value == "1")
}
