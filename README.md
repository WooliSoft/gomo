# Gomo

`gomo` is a Rust CLI for running Gleam package workflows across a monorepo. It
discovers packages from configured project roots such as
`apps/*`, `libs/*`, `services/*`, and `tools/*`, builds a local dependency graph
from Gleam path dependencies, runs tasks in dependency order, and caches
successful build and test tasks.

## Create a Workspace

Create a full-stack Gleam monorepo in a new directory, or initialize the current
directory:

```sh
gomo init my_app
gomo init .
```

The generated starter contains a JavaScript Lustre frontend under `apps/web`, a
shared API contract package under `libs/shared`, and a Wisp/Mist service under
`services/api`. The frontend uses the official `lustre_dev_tools` development
server and proxies `/api` requests to the service. The scaffold also includes an
Ubuntu GitHub Actions workflow, focused tests, and a README with development and
production commands. Package lock manifests are included so CI can run
`gomo deps check`. The scaffold does not generate Nix files, a task runner, or
version control metadata.

`init` refuses to overwrite managed files or merge into existing generated
package directories. Other files already present in the target directory are
left unchanged.

## Local Workflows

From anywhere inside a configured workspace:

```sh
gomo build
gomo test
gomo format
gomo format --check
```

Named workflow tasks compose native Gomo operations, project-backed modules,
structured external commands, shell pipelines, and other tasks:

```sh
gomo task list
gomo task list --project web_app
gomo task run release
gomo task run vite-build --project web_app
gomo task run-many lint --all
```

Workspace tasks are declared under `[tasks.<name>]` in `gomo.toml`. Reusable
project tasks set `scope = "project"` and are exposed explicitly by a package:

```toml
# gomo.toml
[tasks.vite-build]
scope = "project"
description = "Build the Vite-backed application"
cache = true
steps = [
  { shell = { command = "vite build", cwd = "{project_root}" } },
]

[tasks.release]
depends_on = ["validate"]
steps = [
  { task = { name = "vite-build", project = "web_app" } },
  { exec = { program = "docker", args = ["compose", "build"] } },
]

# apps/web_app/gleam.toml
[tools.gomo.tasks.vite-build]
extends = "vite-build"

[tools.gomo.build]
task = "vite-build"
```

Task definitions support sequential or parallel steps, explicit prerequisites,
workspace/project scope, descriptions, persistent workflows, declared inputs
and outputs, and opt-in cache metadata. Each step has exactly one `task`,
`target`, `dev`, `watch`, `module`, `exec`, or `shell` action. Direct project
task invocation always requires `--project`; the current directory never
selects a project implicitly.

Watch finite work across a project and its transitive local dependencies:

```sh
gomo watch --target build --project my_app --with-deps
gomo watch --project my_app --with-deps -- ./scripts/regenerate.sh
```

Watch performs an initial run, debounces filesystem events, serializes runs,
and keeps waiting after a failed task or callback. Generated `build/`, `target/`,
`node_modules/`, `.gomo/`, `.gomo-restore-*`, `.gomo-backups-*`, `.git/`, `.jj/`,
`.devenv/`, and `.direnv/` output is ignored. Callback commands run from the
selected project root and receive `GOMO_CHANGED_FILES`, `GOMO_CHANGED_PROJECTS`,
`GOMO_WATCH_PROJECT`, and JSON companion variables ending in `_JSON`.
Use `--no-initial-run` to skip the initial callback or target run.

Manage a long-running development process with build-before-restart semantics:

```sh
gomo dev --project my_app
gomo dev --project my_app -- gleam run -m my_app
```

`gomo dev` builds the project and its local dependencies before starting the
process, runs it from the project root, and restarts it only after a successful
rebuild. A failed rebuild leaves the current process running. Filesystem
events and debounce timing are provided by Watchexec, and its supervisor owns
the development process and its descendants. Development commands may be
configured in `gleam.toml`:

```toml
[tools.gomo.watch]
command = "./scripts/regenerate.sh"

[tools.gomo.dev]
command = "gleam run"
reload = "restart"
```

The `hot` reload value is accepted as a forward-compatible strategy and falls
back to a managed restart until a runtime helper is connected. Watchexec runs
development commands in their own process group so restart and Ctrl-C clean up
shell descendants as well.

For workspace inspection and troubleshooting:

```sh
gomo doctor
gomo deps check
gomo deps vendor
gomo projects
gomo graph
```

Local build and test task runs use a full-screen Ratatui interface when stdout
is an interactive terminal. It shows per-project status, selected task logs,
current parallel work, progress, and cache counts. Use `↑`/`↓` or `j`/`k` to
select tasks and `L` to view logs fullscreen without side borders for easier
copying. If the finished TUI auto-exits, Gomo prints the captured task logs and
summary in a static format. Gomo automatically uses plain static output when
run by a recognized coding agent, terminal I/O is captured, `TERM=dumb`,
`NO_COLOR` is set, or `CI` is set. Use `--ci` to force static logs or `--json`
for machine-readable summaries.

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

# Optional strict vendoring
[vendoring]
dir = "vendor"
```

`project_roots` supports exact paths and direct-child globs like `apps/*`.
Unknown config fields are rejected so typos do not silently change behavior.
`dependency_versions` is optional. When present, `enabled` defaults to `true`,
`include_local` defaults to `true`, and `ignore` defaults to an empty list.
`[vendoring]` is optional; omit it unless the workspace wants locked external
packages checked into a vendor store.

Project-level target config lives in each package's `gleam.toml` under
`[tools.gomo.<target>]`. `inputs` override the files used for cache keys and
affected-file matching. `command` overrides the command Gomo runs for that
target. Build targets can use `cached_folders` to replace the default
`["build"]` output list with exact project-relative directories:

```toml
[tools.gomo.test]
inputs = ["gleam.toml", "src/**", "test/**", "fixtures/**"]
command = "gleam test --target erlang"

[tools.gomo.build]
task = "vite-build"

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
version matches the referenced local package's `gleam.toml` version. When
`[vendoring]` is configured, the same command also validates the vendor store
(see [Dependency Vendoring](#dependency-vendoring)).

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

## Dependency Vendoring

Add an optional root `[vendoring]` table to enable strict dependency vendoring.
`dir` defaults to `vendor`:

```toml
[vendoring]
dir = "vendor"
```

`gomo deps vendor` synchronizes exact Hex and Git packages from checked-in
`manifest.toml` locks into `gomo-vendor.toml`, `hex`, and `git` under the
configured vendor directory. The directory is `vendor` when `dir` is omitted.
Set `HEXPM_READ_API_KEY` when private Hex packages need a read token.

```sh
gomo deps vendor
gomo deps check
```

With vendoring enabled, `gomo deps check` validates resolved versions plus
vendor inventory, archive checksums, and project snapshots. `gomo doctor`
reports vendoring issues when the table is present.

Build, test, dev, watch, tasks bound to build/test targets, and structured
`module`/`target` steps launched through Gomo fail closed when vendor state is
missing or stale. Raw `gleam` invocations and standalone arbitrary `shell`/`exec`
task steps are outside that guarantee.

`gomo clean` removes project build output but not vendor files. The next Gomo
command that prepares projects hydrates Hex archives into Gleam's checksum cache
and restores Git package freshness state under each project's `build/packages`.

## Cache

Successful `build` and `test` tasks are cached by default. Build cache hits
restore the project's configured cached folders, which default to `build/`;
test cache hits replay the successful test output. Failed test runs are not
cached. Every configured cached folder must be created by a successful build.
Cache restore completely replaces each folder so stale output files cannot
survive a cache hit. Cached folders must be non-overlapping, project-relative
directories without `.` or `..`, and symlinks are not supported within them.

Useful cache controls:

```sh
gomo --cache-mode read-write test
gomo --cache-mode read test
gomo --no-remote-cache build
gomo --remote-cache-read-only test
gomo --require-remote-cache test
gomo --no-cache build
gomo --no-restore test
gomo explain --target test --project web_app
gomo reset --only-cache
```

`off`, `read`, `write`, and `read-write` cache modes are also available through
`GOMO_CACHE_MODE`. The local cache is always checked before a configured remote.
`reset --only-cache` removes the configured local cache directory.

### GitHub Actions snapshot cache

Small projects can snapshot `.gomo/cache` with the supported
`actions/cache/restore@v5` and `actions/cache/save@v5` actions. See
[`examples/github-actions-cache.yml`](examples/github-actions-cache.yml). The
save key is unique per job execution while the restore prefix is stable.
Matrix dimensions that alter inputs or project selection must be included in
both keys. Pull requests should restore without saving a trusted branch key.

This snapshots the entire local cache, is branch-scoped and evictable by
GitHub, and does not merge snapshots from parallel jobs. It is not the
multi-user remote cache protocol and Gomo does not call GitHub's undocumented
cache-service upload APIs.

### Authenticated HTTP remote cache

Configure a service without putting credentials in `gomo.toml`:

```toml
[cache.remote]
backend = "http"
url = "https://cache.example.internal"
workspace = "wooli"
mode = "auto"
failure = "warn"
max_concurrent_transfers = 4
```

Set `GOMO_REMOTE_CACHE_TOKEN` at runtime for static-token authentication.
GitHub Actions jobs may instead grant `id-token: write`; when
`ACTIONS_ID_TOKEN_REQUEST_URL` and `ACTIONS_ID_TOKEN_REQUEST_TOKEN` are
available, Gomo requests and exchanges a GitHub OIDC JWT automatically. Set
`GOMO_REMOTE_CACHE_OIDC_AUDIENCE` when the trust rule's audience differs from
the service URL. No long-lived cache secret is needed in that mode.
`GOMO_REMOTE_CACHE_URL`,
`GOMO_REMOTE_CACHE_WORKSPACE`, `GOMO_REMOTE_CACHE_MODE`, and
`GOMO_REMOTE_CACHE_RUN_ID` can override CI-specific settings. Tokens, S3
credentials, and cached secret files must never be committed. Use a read-only
identity for developers and untrusted CI; only protected CI should receive
`cache:shared:write`. Release and deployment jobs that require fresh provenance
should use `--no-remote-cache` and must not treat a cache hit as a deployable
artifact attestation.

HTTPS uses the platform certificate store. Install an internal cache service's
root CA in the operating system trust store instead of disabling certificate
validation.

The server is a separate binary:

```sh
gomo-cache-server migrate
gomo-cache-server doctor
gomo-cache-server serve
```

It uses a local SQLite database as the first-writer-wins publication authority
and an S3-compatible bucket only for immutable bundle bytes. The cache server is
single-process by design; SQLite uses exclusive locking to prevent a second
server or administration command from opening the database concurrently. The
serving process applies embedded migrations at startup and runs retention
garbage collection itself.
Garage is supported with region `garage`, a custom endpoint, and path-style
addressing. Clients never receive bucket credentials.

## CI Workflows

Non-interactive commands automatically avoid rich terminal rendering. Use
`--ci` to force plain output even in a terminal and `--json` for
machine-readable summaries:

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
- Stale or missing vendor store: run `gomo deps check`, then `gomo deps vendor`.
- Cache confusion: run `gomo explain --target <build|test> --project <name>` to inspect cache inputs, or `gomo reset --only-cache` to remove local entries.

## License

MIT. See `LICENSE`.
