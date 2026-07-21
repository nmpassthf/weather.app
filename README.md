# Weather App

Weather App is a cross-platform weather client written in Rust. It ships as
one BusyBox-style `weather.app` executable with optional daemon, terminal, and
desktop interfaces.

Weather data comes from the NMC provider and is served through a local engine.
The GUI and TUI share the same configuration, cache, and engine protocol.

![TUI screenshot](https://raw.githubusercontent.com/nmpassthf/weather.app/refs/heads/master/docs/README/main.png)

## Features

- One executable for GUI, TUI, and daemon modes.
- Desktop GUI for Linux, Windows 10, and macOS.
- Interactive TUI and one-shot text or JSON output.
- Station search, management, forecasts, alerts, and temperature history.
- Local caching, background refresh, proxy support, and stale-data fallback.
- systemd user or system service installation on Linux.

The executable selects its interface from the launch environment:

```sh
weather.app                  # TUI in a terminal; GUI on desktop launch
weather.app gui              # Explicit GUI
weather.app tui              # Explicit TUI
weather.app tui once         # One-shot weather output
weather.app tui --format json once
weather.app daemon status    # Control the local engine
```

Renaming or symlinking the executable to `weather-daemon`, `weather-tui`, or
`weather-gui` selects that mode directly.

Configuration defaults to `~/.weather/config/weather.toml`. Use
`-c/--config <path>` to select another file.

## Build

Requirements:

- Stable Rust toolchain.
- Bun 1.3.14 for GUI development and packaging.
- Native Tauri dependencies for the target platform.

Ubuntu 22.04 is the minimum supported Linux build target. See the
[GUI development notes](weather-renderer/gui/README.md) for platform packages
and bundle prerequisites.

The application features are:

| Feature | Contents |
| --- | --- |
| `daemon` | Local engine and daemon commands; enabled by default |
| `tui` | Terminal interface |
| `gui` | Tauri desktop interface |
| `desktop` | `daemon`, `tui`, and `gui` |

Build a daemon-only binary:

```sh
cargo build -p weather-app
```

Build the terminal application with its embedded daemon:

```sh
cargo build -p weather-app --features tui
```

Build a standalone desktop debug executable with embedded frontend assets:

```sh
cd weather-renderer/gui
bun install --frozen-lockfile
bun run standalone:debug
```

Cargo uses the target name `weather-app`; Make and Tauri publish it as
`weather.app` (or `weather.app.exe` on Windows).

Build optimized single-binary variants with Make:

```sh
make             # Native daemon + TUI + GUI (default)
make gui         # Native daemon + GUI
make tui         # Native daemon + TUI
make musl-tui    # Static x86_64 musl daemon + TUI
```

Artifacts and checksums are written to
`target/release-artifacts/<target-triple>/<variant>/`. The musl build requires
the `x86_64-unknown-linux-musl` Rust target and a compatible musl C compiler;
override `MUSL_TARGET` and `MUSL_CC` for another toolchain.

Build a native desktop bundle:

```sh
cd weather-renderer/gui
bun install --frozen-lockfile
bun run bundle
```

After CI succeeds on `master`, GitHub Actions produces Linux, Windows, and
macOS bundles in the `weather-desktop-latest` artifact. Only the newest
complete successful bundle set is retained.

## Development

The main workspace areas are:

```text
weather-core/          engine, daemon, storage, provider, and configuration
weather-renderer/tui/  terminal interface
weather-renderer/gui/  Tauri and web frontend
weather-schema/        shared engine protocol
```

Run the GUI with Vite hot reload:

```sh
cd weather-renderer/gui
bun install --frozen-lockfile
bun run tauri dev
```

`bun run tauri dev` automatically compiles the `desktop` feature set and
launches GUI mode.

Run the project checks before submitting changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features

cd weather-renderer/gui
bun test
bun run build
```

Install the daemon as a Linux user service when needed:

```sh
weather.app daemon service install systemd
```

## License

WTFPL.
