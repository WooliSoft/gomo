# Gleam Full-Stack Starter

A small full-stack Gleam monorepo managed by [Gomo](https://github.com/WooliSoft/gomo):

- `apps/web` is a JavaScript frontend built with Lustre.
- `libs/shared` contains API types and JSON codecs shared across targets.
- `services/api` is an Erlang service built with Wisp and Mist.

## Prerequisites

- Gleam 1.17 or newer
- Gomo
- Erlang/OTP
- Node.js for JavaScript-target tests

Lustre's development tools download and manage their own Bun binary. On Linux,
install `inotify-tools` to enable filesystem-event-based live reload. The server
still works without it.

## Development

Start the API in one terminal:

```sh
cd services/api
gleam run
```

Start the Lustre development server in another terminal:

```sh
cd apps/web
gleam run -m lustre/dev start
```

Open <http://localhost:1234>. Lustre proxies `/api` requests to the API service
at `http://localhost:3000` and reloads when frontend or shared code changes.

## Workspace Commands

Run these commands from anywhere in the repository:

```sh
gomo doctor
gomo projects
gomo deps check
gomo build
gomo test
gomo format
gomo format --check
```

## Production Build

Build the minified frontend into the API service's `priv/static` directory:

```sh
cd apps/web
gleam run -m lustre/dev build
```

Then start the service and open <http://localhost:3000>:

```sh
cd services/api
gleam run
```
