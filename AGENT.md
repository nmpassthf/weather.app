# Agent Guidance

## Repository Overview

This is a Rust 2024 workspace organized around three top-level layers: protocol, core backend, and frontend renderers. The root `Cargo.toml` manages all crates. The single public binary is `weather.app` (the internal Cargo target is `weather-app`). It dispatches daemon, TUI, and GUI modes by subcommand, launch environment, or BusyBox-style `argv[0]` aliases.

`Makefile` is mainly for static release builds. `scripts/` contains NMC upstream inspection scripts. `todo/` and `docs/superpowers/` are historical design and planning material; current code structure takes priority.

## Layering

### `weather-schema`

Protocol and shared type layer. It defines the protobuf schema, ZMQ RPC/Event envelopes, encoding/decoding helpers, schema constants, and HMAC utilities. Shared wire/domain types should live here so renderer, engine, db, and updater do not each invent their own structures.

### `weather-core`

Core backend layer. It contains configuration, persistence, upstream updates, engine orchestration, and daemon/supervisor components. Core owns business state, configuration structures, cache/refresh behavior, DB writes, upstream requests, and engine lifecycle.

#### `weather-core/conf`

Configuration layer. The crate name is `weather-configure`. It owns TOML config types, defaults, loading, validation, config state, and conversion between config structures and protobuf structures. Frontends must not write config files directly; config reads and writes should go through structured operations exposed by engine.

#### `weather-core/db`

Persistence layer. The crate name is `weather-db`. It owns SQLite/rusqlite storage, cache tables, and the DB actor. In normal application flow, only engine owns and writes to DB.

#### `weather-core/updater` and `weather-core/utils`

Upstream access and shared utility layer. `weather-updater` owns provider, catalog, and HTTP request logic. `weather-utils` holds general utility code. Upstream raw response models should stay in updater and must not leak into renderer.

#### `weather-core/engine`

Core orchestration layer. Engine composes config, DB actor, updater, refresh logic, and RPC handlers, then exposes service through ZMQ RPC/PUB. Weather queries, region search, structured config reads/writes, status, refresh, restart, and event responses should all be exposed from engine.

#### `weather-core/daemon`

Process and service management layer. Daemon provides `run`, `probe`, and `service` commands. It decides whether an engine is already running, starts the foreground supervisor, and installs/removes system services. Singleton detection, lock-file handling, and service lifecycle belong on the daemon/engine side, not in renderer.

### `weather-renderer`

Public frontend family, not a synonym for TUI. `weather-renderer/common` owns the engine client, pagination, and daemon supervision shared by renderers. `weather-renderer/tui` is a library used by the optional `tui` application feature. `weather-renderer/gui/src-tauri` owns the `weather-app` package and optional Tauri `gui` feature. Both work through schema/engine interfaces.

## Runtime Relationship

The default runtime path is:

```text
weather.app GUI/TUI -> weather.app daemon probe -> weather-engine(ZMQ RPC/PUB)
```

If `probe` finds an existing engine, renderer uses the returned endpoint. If no engine is running and the `daemon` feature is compiled, a renderer starts the same executable as `weather.app daemon run --foreground`, then calls engine over ZMQ. Daemon mode and foreground mode only change lifecycle ownership; they must not change engine business semantics.

IPC uses ZMQ by default, with protobuf payloads from `weather-schema`. JSON output should also come from engine/schema structures, not directly from upstream NMC raw JSON.

## Development Boundaries

- Renderer is a frontend family. It should handle user entry points, engine client calls, and presentation; it should not contain upstream requests, DB access, or core lifecycle internals.
- TUI is only the current renderer implementation. Do not describe all of `weather-renderer` as if it only serves TUI.
- Config file reading, validation, updates, and persistence should be exposed by engine/core through structured interfaces. Frontends may request, display, and submit config structure changes, but must not write TOML files directly.
- Core owns business and runtime state. Core subcomponent notes stay under the `weather-core` fourth-level headings and should remain brief.
- Schema changes must preserve protobuf field compatibility; use `reserved` for removed fields.
- Tests should not depend on live NMC network calls by default. Use scripts under `scripts/` when upstream inspection is needed.

## Common Checks

After a complete feature or bug fix, run at least:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

For documentation-only or small explanatory changes, choose a lighter check based on the actual impact.
