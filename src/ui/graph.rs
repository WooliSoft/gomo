use std::collections::{BTreeMap, BTreeSet};

use crate::{
    graph::{ProjectGraph, UnmanagedPathDependency},
    workspace::Workspace,
};

const RESET: &str = "\x1b[0m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_GREEN: &str = "\x1b[1;32m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const DIM_GRAY: &str = "\x1b[2;90m";
const MAGENTA: &str = "\x1b[35m";

pub(crate) fn render(workspace: &Workspace, graph: &ProjectGraph) -> String {
    if workspace.projects.is_empty() {
        return format!(
            "No Gleam projects found under {}.\n",
            workspace.project_globs.join(", ")
        );
    }

    let mut output = format!("{}Dependency Graph{}\n", BOLD_CYAN, RESET);

    for line in render_graph_lines(graph) {
        output.push_str(&line);
        output.push('\n');
    }

    output
}

pub(crate) fn render_plain(workspace: &Workspace, graph: &ProjectGraph) -> String {
    if workspace.projects.is_empty() {
        return format!(
            "No Gleam projects found under {}.\n",
            workspace.project_globs.join(", ")
        );
    }

    let mut output = String::new();
    output.push_str("Dependency Graph\n");
    for project in &graph.topological_order {
        output.push_str(&format!("- {project}\n"));
        output.push_str(&format!(
            "  upstream: {}\n",
            render_name_list(graph.upstream_for(project))
        ));
        output.push_str(&format!(
            "  downstream: {}\n",
            render_name_list(graph.downstream_for(project))
        ));
        if !graph.unmanaged_for(project).is_empty() {
            output.push_str(&format!(
                "  unmanaged: {}\n",
                render_unmanaged(graph.unmanaged_for(project))
            ));
        }
    }
    output
}

fn render_name_list(projects: &[String]) -> String {
    if projects.is_empty() {
        "-".to_string()
    } else {
        projects.join(", ")
    }
}

fn render_graph_lines(graph: &ProjectGraph) -> Vec<String> {
    let ranks = graph
        .topological_order
        .iter()
        .enumerate()
        .map(|(index, project)| (project.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut remaining_downstream = graph
        .topological_order
        .iter()
        .map(|project| (project.clone(), graph.downstream_for(project).len()))
        .collect::<BTreeMap<_, _>>();
    let mut ready = remaining_downstream
        .iter()
        .filter_map(|(project, count)| (*count == 0).then_some(project.clone()))
        .collect::<BTreeSet<_>>();
    let mut rendered = BTreeSet::<String>::new();
    let mut lanes = Vec::<String>::new();
    let mut lines = Vec::<String>::new();

    while rendered.len() < graph.topological_order.len() {
        let lane_index = match next_ready_lane(&lanes, &ready, &rendered) {
            Some(index) => index,
            None => {
                let project = next_ready_project(&ready, &rendered, &lanes, &ranks)
                    .or_else(|| next_unrendered_project(graph, &rendered, &ranks))
                    .expect("unrendered project exists while graph rendering is incomplete");
                lanes.push(project);
                lanes.len() - 1
            }
        };
        let project = lanes[lane_index].clone();
        let upstream = sorted_upstream(graph.upstream_for(&project), &rendered, &ranks);

        lines.push(render_node_line(graph, &lanes, lane_index, &project));

        ready.remove(&project);
        rendered.insert(project.clone());
        for dependency in graph.upstream_for(&project) {
            let count = remaining_downstream
                .get_mut(dependency)
                .expect("dependency project exists in downstream count map");
            *count -= 1;
            if *count == 0 && !rendered.contains(dependency) {
                ready.insert(dependency.clone());
            }
        }

        let next_lanes = next_lanes(lanes.clone(), lane_index, upstream.clone(), &rendered);
        if let Some(line) = render_transition_line(&lanes, lane_index, &upstream, &next_lanes) {
            lines.push(line);
        }
        lanes = next_lanes;
    }

    lines
}

fn next_ready_lane(
    lanes: &[String],
    ready: &BTreeSet<String>,
    rendered: &BTreeSet<String>,
) -> Option<usize> {
    lanes
        .iter()
        .position(|project| ready.contains(project) && !rendered.contains(project))
}

fn next_ready_project(
    ready: &BTreeSet<String>,
    rendered: &BTreeSet<String>,
    lanes: &[String],
    ranks: &BTreeMap<String, usize>,
) -> Option<String> {
    ready
        .iter()
        .filter(|project| !rendered.contains(*project) && !lanes.contains(*project))
        .max_by_key(|project| ranks.get(*project).copied().unwrap_or_default())
        .cloned()
}

fn next_unrendered_project(
    graph: &ProjectGraph,
    rendered: &BTreeSet<String>,
    ranks: &BTreeMap<String, usize>,
) -> Option<String> {
    graph
        .topological_order
        .iter()
        .filter(|project| !rendered.contains(*project))
        .max_by_key(|project| ranks.get(*project).copied().unwrap_or_default())
        .cloned()
}

fn sorted_upstream(
    upstream: &[String],
    rendered: &BTreeSet<String>,
    ranks: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut upstream = upstream
        .iter()
        .filter(|project| !rendered.contains(*project))
        .cloned()
        .collect::<Vec<_>>();

    upstream.sort_by(|left, right| {
        ranks
            .get(right)
            .cmp(&ranks.get(left))
            .then_with(|| left.cmp(right))
    });

    upstream
}

fn render_node_line(
    graph: &ProjectGraph,
    lanes: &[String],
    lane_index: usize,
    project: &str,
) -> String {
    let mut line = render_lane_prefix(lanes.len(), lane_index);
    line.push_str(&style(project, BOLD_YELLOW));

    if !graph.unmanaged_for(project).is_empty() {
        line.push_str("  unmanaged: ");
        line.push_str(&style(
            &render_unmanaged(graph.unmanaged_for(project)),
            MAGENTA,
        ));
    }

    line
}

fn render_lane_prefix(lane_count: usize, active_lane: usize) -> String {
    let mut prefix = String::new();

    for index in 0..lane_count {
        if index == active_lane {
            prefix.push_str(&style("○", BOLD_GREEN));
        } else {
            prefix.push_str(&style("│", DIM_GRAY));
        }
        prefix.push(' ');
    }

    prefix
}

fn next_lanes(
    mut lanes: Vec<String>,
    lane_index: usize,
    upstream: Vec<String>,
    rendered: &BTreeSet<String>,
) -> Vec<String> {
    lanes.remove(lane_index);
    let mut insert_at = lane_index.min(lanes.len());

    for dependency in upstream {
        if rendered.contains(&dependency) || lanes.contains(&dependency) {
            continue;
        }

        lanes.insert(insert_at, dependency);
        insert_at += 1;
    }

    lanes
}

fn render_transition_line(
    old_lanes: &[String],
    active_lane: usize,
    upstream: &[String],
    new_lanes: &[String],
) -> Option<String> {
    let width = old_lanes.len().max(new_lanes.len()).saturating_mul(2);
    if width == 0 {
        return None;
    }

    let mut cells = vec![Directions::empty(); width.saturating_sub(1)];

    for (old_index, project) in old_lanes.iter().enumerate() {
        if old_index == active_lane {
            for dependency in upstream {
                if let Some(new_index) = new_lanes
                    .iter()
                    .position(|candidate| candidate == dependency)
                {
                    draw_edge(&mut cells, old_index, new_index);
                }
            }
        } else if let Some(new_index) = new_lanes.iter().position(|candidate| candidate == project)
        {
            draw_edge(&mut cells, old_index, new_index);
        }
    }

    let line = cells
        .into_iter()
        .map(Directions::to_char)
        .collect::<String>()
        .trim_end()
        .to_string();

    if line.is_empty()
        || line
            .chars()
            .all(|character| character == '│' || character == ' ')
    {
        None
    } else {
        Some(style_graph_line(&line))
    }
}

fn style(text: &str, ansi: &str) -> String {
    format!("{ansi}{text}{RESET}")
}

fn style_graph_line(line: &str) -> String {
    let mut styled = String::new();

    for character in line.chars() {
        if character == ' ' {
            styled.push(character);
        } else {
            styled.push_str(DIM_GRAY);
            styled.push(character);
            styled.push_str(RESET);
        }
    }

    styled
}

fn draw_edge(cells: &mut [Directions], old_index: usize, new_index: usize) {
    let from = old_index * 2;
    let to = new_index * 2;

    if from == to {
        cells[from].insert(Directions::UP | Directions::DOWN);
        return;
    }

    let start = from.min(to);
    let end = from.max(to);

    if from < to {
        cells[from].insert(Directions::UP | Directions::RIGHT);
        for cell in cells.iter_mut().take(end).skip(start + 1) {
            cell.insert(Directions::LEFT | Directions::RIGHT);
        }
        cells[to].insert(Directions::LEFT | Directions::DOWN);
    } else {
        cells[from].insert(Directions::UP | Directions::LEFT);
        for cell in cells.iter_mut().take(end).skip(start + 1) {
            cell.insert(Directions::LEFT | Directions::RIGHT);
        }
        cells[to].insert(Directions::RIGHT | Directions::DOWN);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Directions(u8);

impl Directions {
    const UP: Self = Self(1 << 0);
    const DOWN: Self = Self(1 << 1);
    const LEFT: Self = Self(1 << 2);
    const RIGHT: Self = Self(1 << 3);

    fn empty() -> Self {
        Self(0)
    }

    fn insert(&mut self, direction: Self) {
        self.0 |= direction.0;
    }

    fn contains(self, direction: Self) -> bool {
        self.0 & direction.0 == direction.0
    }

    fn to_char(self) -> char {
        let up = self.contains(Self::UP);
        let down = self.contains(Self::DOWN);
        let left = self.contains(Self::LEFT);
        let right = self.contains(Self::RIGHT);

        match (up, down, left, right) {
            (false, false, false, false) => ' ',
            (true, false, false, false)
            | (false, true, false, false)
            | (true, true, false, false) => '│',
            (false, false, true, false)
            | (false, false, false, true)
            | (false, false, true, true) => '─',
            (true, false, false, true) => '╰',
            (true, false, true, false) => '╯',
            (false, true, false, true) => '╭',
            (false, true, true, false) => '╮',
            (true, true, false, true) => '├',
            (true, true, true, false) => '┤',
            (false, true, true, true) => '┬',
            (true, false, true, true) => '┴',
            (true, true, true, true) => '┼',
        }
    }
}

impl std::ops::BitOr for Directions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

fn render_unmanaged(dependencies: &[UnmanagedPathDependency]) -> String {
    if dependencies.is_empty() {
        return "-".to_string();
    }

    dependencies
        .iter()
        .map(|dependency| {
            format!(
                "{} ({})",
                dependency.name,
                dependency.root_relative_path.display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
