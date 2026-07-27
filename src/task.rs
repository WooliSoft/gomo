use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use anyhow::{Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskScope {
    Workspace,
    Project,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
    #[default]
    Sequential,
    Parallel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    pub scope: TaskScope,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
    pub mode: TaskMode,
    pub steps: Vec<TaskStep>,
    pub persistent: bool,
    pub cache: bool,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub env_inputs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinition {
    pub extends: Option<String>,
    pub scope: Option<TaskScope>,
    pub description: Option<String>,
    pub depends_on: Option<Vec<String>>,
    pub mode: Option<TaskMode>,
    pub steps: Option<Vec<TaskStep>>,
    pub persistent: Option<bool>,
    pub cache: Option<bool>,
    pub inputs: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
    pub env_inputs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskStep {
    pub task: Option<TaskAction>,
    pub target: Option<TargetAction>,
    pub dev: Option<DevAction>,
    pub watch: Option<WatchAction>,
    pub module: Option<ModuleAction>,
    pub exec: Option<ExecAction>,
    pub shell: Option<ShellAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAction {
    pub name: String,
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetAction {
    pub target: String,
    pub project: Option<String>,
    #[serde(default)]
    pub with_deps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevAction {
    pub project: String,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchAction {
    pub target: String,
    pub project: Option<String>,
    #[serde(default = "default_true")]
    pub initial_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleAction {
    pub project: String,
    pub module: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecAction {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellAction {
    pub command: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

impl TaskStep {
    pub fn action_name(&self) -> Result<&'static str> {
        let actions = [
            ("task", self.task.is_some()),
            ("target", self.target.is_some()),
            ("dev", self.dev.is_some()),
            ("watch", self.watch.is_some()),
            ("module", self.module.is_some()),
            ("exec", self.exec.is_some()),
            ("shell", self.shell.is_some()),
        ];
        let selected = actions
            .iter()
            .filter_map(|(name, selected)| selected.then_some(*name))
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            bail!(
                "each task step must define exactly one action, found {}",
                if selected.is_empty() {
                    "none".to_string()
                } else {
                    selected.join(", ")
                }
            );
        }
        Ok(selected[0])
    }

    pub fn is_orchestration(&self) -> bool {
        self.task.is_some() || self.dev.is_some() || self.watch.is_some()
    }

    pub fn is_automatically_persistent(&self) -> bool {
        self.dev.is_some() || self.watch.is_some()
    }
}

impl TaskDefinition {
    pub fn resolve(
        &self,
        name: &str,
        default_scope: TaskScope,
        base: Option<&Task>,
    ) -> Result<Task> {
        let task = Task {
            name: name.to_string(),
            scope: self
                .scope
                .or_else(|| base.map(|task| task.scope))
                .unwrap_or(default_scope),
            description: self
                .description
                .clone()
                .or_else(|| base.and_then(|task| task.description.clone())),
            depends_on: self
                .depends_on
                .clone()
                .or_else(|| base.map(|task| task.depends_on.clone()))
                .unwrap_or_default(),
            mode: self
                .mode
                .or_else(|| base.map(|task| task.mode))
                .unwrap_or_default(),
            steps: self
                .steps
                .clone()
                .or_else(|| base.map(|task| task.steps.clone()))
                .unwrap_or_default(),
            persistent: self
                .persistent
                .or_else(|| base.map(|task| task.persistent))
                .unwrap_or(false),
            cache: self
                .cache
                .or_else(|| base.map(|task| task.cache))
                .unwrap_or(false),
            inputs: self
                .inputs
                .clone()
                .or_else(|| base.map(|task| task.inputs.clone()))
                .unwrap_or_default(),
            outputs: self
                .outputs
                .clone()
                .or_else(|| base.map(|task| task.outputs.clone()))
                .unwrap_or_default(),
            env_inputs: self
                .env_inputs
                .clone()
                .or_else(|| base.map(|task| task.env_inputs.clone()))
                .unwrap_or_default(),
        };
        task.validate()?;
        Ok(task)
    }
}

impl Task {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.steps.is_empty() {
            bail!("task `{}` must define at least one step", self.name);
        }
        for step in &self.steps {
            step.action_name()
                .map_err(|error| anyhow::anyhow!("task `{}`: {error}", self.name))?;
        }
        if self.cache && self.persistent {
            bail!("task `{}` cannot be both cached and persistent", self.name);
        }
        if self.cache && self.steps.iter().any(TaskStep::is_orchestration) {
            bail!(
                "task `{}` cannot enable caching because it contains a task, dev, or watch action",
                self.name
            );
        }
        if self.steps.iter().any(TaskStep::is_automatically_persistent) && !self.persistent {
            bail!(
                "task `{}` contains a dev or watch action and must set persistent = true",
                self.name
            );
        }
        if self.mode == TaskMode::Sequential {
            if let Some((index, _)) = self
                .steps
                .iter()
                .enumerate()
                .find(|(_, step)| step.is_automatically_persistent())
            {
                if index + 1 != self.steps.len() {
                    bail!(
                        "task `{}` has a dev or watch action before the final step; automatically persistent steps must be last in sequential tasks",
                        self.name
                    );
                }
            }
        }
        for path in self.inputs.iter().chain(&self.outputs) {
            validate_declared_path(&self.name, path)?;
        }
        let mut env = BTreeSet::new();
        for name in &self.env_inputs {
            if name.trim().is_empty() {
                bail!("task `{}` has an empty env_inputs entry", self.name);
            }
            if !env.insert(name) {
                bail!("task `{}` repeats env input `{name}`", self.name);
            }
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.chars().enumerate().all(|(index, character)| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || (character == '-' && index > 0)
        })
        || name.ends_with('-')
        || name.contains("--")
    {
        bail!("invalid task name `{name}`; task names must use kebab-case");
    }
    Ok(())
}

fn validate_declared_path(task: &str, value: &str) -> Result<()> {
    let value = value
        .replace("{workspace_root}", "workspace")
        .replace("{project_root}", "project");
    let path = Path::new(&value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "task `{task}` has invalid path `{value}`; task paths must stay inside the workspace"
        );
    }
    Ok(())
}
