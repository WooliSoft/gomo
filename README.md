# Gomo

`gomo` is a Rust CLI for running Gleam package workflows across a monorepo. It
discovers packages from configured project roots such as
`apps/*`, `libs/*`, `services/*`, and `tools/*`, builds a local dependency graph
from Gleam path dependencies, runs tasks in dependency order, and caches
successful build and test tasks.

## Local Workflows

From anywhere inside a configured workspace:

```sh
gomo build
gomo test
gomo format
gomo format --check
```

For workspace inspection and troubleshooting:

```sh
gomo doctor
gomo deps check
gomo projects
gomo graph
```

Local build and test task runs use a full-screen Ratatui interface when stdout
is an interactive terminal. It shows per-project status, selected task logs,
current parallel work, progress, and cache counts. Use `↑`/`↓` or `j`/`k` to
select tasks and `L` to view logs fullscreen without side borders for easier
copying. If the finished TUI auto-exits, Gomo prints the captured task logs and
summary in a static format. Use `--ci` for static logs or `--json` for
machine-readable summaries.

Run one project, or include its upstream local dependencies:

```sh
gomo run --target build --project web_app
gomo run --target test --project web_app --with-deps
```

Run affected validation from an explicit changed-file list, or from VCS changes
against a base ref:

```sh
gomo affected --target test --files libs/shared/src/widget.gleam
gomo affected --target test --base main
```

`affected --base` uses Jujutsu when `.jj` exists at the workspace root, otherwise
Git when `.git` exists. Use `--files` to bypass VCS discovery.

Workspace-level target inputs can make root files affect every project for a
target. They are matched relative to the workspace root and also participate in
task cache keys:

```toml
[workspace.test]
inputs = ["gomo.toml", "devenv.nix", ".github/workflows/**"]
```

Workspace discovery and default task concurrency are configured in `gomo.toml`:

```toml
[workspace]
project_roots = ["apps/*", "libs/*", "services/*", "tools/*"]
default_parallelism = "auto"

[dependency_versions]
enabled = true
include_local = true
ignore = []
```

`project_roots` supports exact paths and direct-child globs like `apps/*`.
Unknown config fields are rejected so typos do not silently change behavior.
`dependency_versions` is optional. When present, `enabled` defaults to `true`,
`include_local` defaults to `true`, and `ignore` defaults to an empty list.

Project-level target config lives in each package's `gleam.toml` under
`[tools.gomo.<target>]`. `inputs` override the files used for cache keys and
affected-file matching. `command` overrides the command Gomo runs for that
target:

```toml
[tools.gomo.test]
inputs = ["gleam.toml", "src/**", "test/**", "fixtures/**"]
command = "gleam test --target erlang"

[tools.gomo.format]
command = "mise exec -- gleam format"

[tools.gomo.format.check]
command = "mise exec -- gleam format --check"
```

Custom commands run through `sh`, so shell syntax such as `&&`, pipes,
redirects, quoting, and environment variable expansion is supported.

Default commands are `gleam build`, `gleam test`, `gleam format`, and
`gleam format --check`. Custom format commands must be configured as a pair:
if `[tools.gomo.format].command` is set, `[tools.gomo.format.check].command`
must also be set, and vice versa.

## Dependency Versions

`gomo deps check` validates resolved dependency versions from each project's
`manifest.toml`. It intentionally checks the lock manifest instead of comparing
version ranges in `gleam.toml`, because the resolved version is the version that
was actually built and tested.

```sh
gomo deps check
gomo deps check --json
```

For Hex packages, the same dependency name must resolve to one version across
all checked manifests. Git packages must resolve to the same version, repository
URL, and commit. For local packages, Gomo also verifies that the locked local
version matches the referenced local package's `gleam.toml` version.

Automatic `doctor` enforcement is controlled from root `gomo.toml`:

```toml
[dependency_versions]
enabled = true
include_local = true
ignore = ["some_intentional_exception"]
```

If the table is absent, `gomo deps check` still works explicitly, but
`gomo doctor` skips dependency version policy checks. Set `enabled = false` to
keep the table's `include_local` or `ignore` settings for explicit checks while
leaving `doctor` unchanged.

## Cache

Successful `build` and `test` tasks are cached by default. Build cache hits
restore the project's `build/` directory; test cache hits replay the successful
test output. Failed test runs are not cached.

Useful cache controls:

```sh
gomo --no-cache build
gomo --no-restore test
gomo explain --target test --project web_app
gomo reset --only-cache
```

`reset --only-cache` removes the configured local cache directory. Cache pruning
is intentionally deferred until the repo has a real retention policy.

## CI Workflows

Use `--ci` to avoid rich terminal rendering and `--json` for machine-readable
summaries:

```sh
gomo --ci doctor
gomo --json projects
gomo --json run-many --target test --all
gomo --json affected --target build --base main
```

`--json` implies CI-friendly rendering for commands that would otherwise use
terminal UI. Task-running JSON output reports the selected target, totals,
cache hit/miss/bypass counts, and each task status.

## Troubleshooting

Start with:

```sh
gomo doctor
```

Common fixes:

- Missing workspace: run from inside a repo containing root `gomo.toml`.
- Unknown project: check `gomo projects` for discovered package names.
- Invalid graph: check local path dependencies in each package's `gleam.toml`.
- Cache confusion: run `gomo explain --target <build|test> --project <name>` to inspect cache inputs, or `gomo reset --only-cache` to remove local entries.

## License

MIT. See `LICENSE`.
