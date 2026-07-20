# Weather App

A layered Rust weather client and local weather service. The project publishes
one BusyBox-style `weather.app` executable containing feature-gated daemon,
TUI, and Tauri GUI modes. A local `weather-engine` runs through the
`weather.app daemon` subcommand.
Weather data is fetched from the NMC provider, cached locally, and exposed to
frontends through the shared `weather-schema` protocol.

TUI screenshots:

*Main screen*

![main screenshot](https://raw.githubusercontent.com/nmpassthf/weather.app/refs/heads/master/docs/README/main.png)

*Search menu*

![search screenshot](https://raw.githubusercontent.com/nmpassthf/weather.app/refs/heads/master/docs/README/search.png)

## Usage

Build the application with TUI support, then run it from a terminal:

```sh
cargo build -p weather-app --features tui
./target/debug/weather-app
```

Common commands:

```sh
# One-shot weather output
weather.app tui once

# JSON output
weather.app tui --format json once

# Force a fresh weather fetch
weather.app tui once --refresh

# Search for a station
weather.app tui stations search "北京"

# Add or update the configured station through engine APIs
weather.app tui stations list
weather.app tui stations add "北京-北京市"

# Show engine status or stop the active engine
weather.app tui engine status
weather.app tui engine stop

# Control the daemon directly
weather.app daemon status
weather.app daemon stop
```

Use `-c/--config <path>` to select a config file. By default, the app uses
`~/.weather/config/weather.toml`. Frontends should request and submit structured
config changes through the engine; they should not edit the TOML file directly
during normal operation.

Useful config commands:

```sh
weather.app tui config defaults
weather.app tui config show
weather.app tui engine restart
```

### Engine logging

New configurations default to engine log level `info`:

```toml
[engine]
request_timeout_ms = 3000
startup_timeout_ms = 8000
lock_path = "engine.lock"
log_level = "info"
```

Accepted levels are `off`, `error`, `warn`, `info`, `debug`, and `trace`.
`info` includes daemon/engine lifecycle and RPC/PUB client connection and
disconnection records. `debug` adds RPC kind, request ID, payload size, result,
and elapsed time together with cache/provider operations; `trace` adds event
publishing and fine-grained socket state. Client identities are represented by
a short hash and their raw bytes are not logged.

Override the configured level for one daemon process by passing the option
after `run`:

```sh
weather.app daemon run --log-level debug
```

The command-line option takes precedence over `[engine].log_level` for the
lifetime of that process. Without an override, an engine restart reloads the
configured level. The built-in logger accepts only project `weather_*` targets,
so Tokio, ZMQ, HTTP client, and other framework logs are suppressed by default.

Network defaults live under `updater.network`. Missing proxy fields inherit the
matching process `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`
variables; an explicitly empty string clears the inherited value.
HTTP(S), SOCKS4/4A, and SOCKS5/5H proxy URLs are supported.

```toml
[updater.network]
http_proxy = "http://127.0.0.1:8123"
https_proxy = "http://127.0.0.1:8123"
no_proxy = "localhost,127.0.0.1"
all_proxy = ""
allow_insecure = false

[[updater.provider]]
name = "nmc"
base_url = "https://www.nmc.cn"
request_timeout_seconds = 20

# Optional per-provider, field-by-field overrides.
[updater.provider.network]
https_proxy = "http://nmc-proxy.example:8123"
no_proxy = "localhost,127.0.0.1,.internal.example"
```

Omitted provider network fields inherit `updater.network`; an explicitly empty
provider value clears that global value. `allow_insecure = true` disables TLS
certificate validation and should only be used for a trusted intercepting
proxy. Changing global or provider network settings requires an engine restart.
`engine.request_timeout_ms` remains the short budget for local/control RPCs.
Weather and catalog RPCs derive a larger end-to-end budget from the active
provider's `request_timeout_seconds`, so a cold catalog plus weather fetch is
not cancelled by the control-plane timeout.
New configurations use a 600-second weather TTL. The refresh scheduler treats
the two-level parent of every enabled three-level station as implicitly
tracked, refreshes that parent before the child, and stores both snapshots in
the engine database without adding the parent to the user's station config.

## GUI

The cross-platform desktop renderer lives in `weather-renderer/gui`. It keeps
feature parity with the interactive TUI for weather display, station search and
management, refresh, engine status/events, config inspection, restart, and
stop. It uses the same protobuf/ZMQ client layer and never accesses provider or
database internals directly. GUI dependencies and scripts use Bun, with the
expected version pinned in `weather-renderer/gui/package.json`.
Weather alerts are a list. Three-level station responses keep their own alerts
first, then append deduplicated alerts inherited from their two-level parent;
two-level stations do not inherit from a synthetic one-level parent.
The GUI requests the current station's DB-backed daily temperature series in
date-cursor pages through `GET_TEMPERATURE_HISTORY`; the engine reduces stored
snapshots to historical daily highs/lows and appends the latest forecast only
to the first page without exposing database rows to the renderer. The chart
opens on two historical days through seven forecast days, fills the available
card width, and loads every remaining history page as the user scrolls toward
the past. It also supports desktop hover, mobile long-press/drag, and keyboard
point inspection. GUI and TUI multi-day views label yesterday, today, and
tomorrow relatively, use weekdays for days +2 through +7, and use concrete
calendar dates outside that window.
Remote images are represented by opaque engine resource IDs rather than NMC
URLs. `GET_RESOURCE` starts an upstream download asynchronously and returns a
short `PENDING` response while it is in flight; once ready, clients pull the
binary data in bounded offset-based chunks. This keeps provider download time
outside the normal engine RPC timeout. The engine caches at most 32 MiB in
memory for 15 minutes and never stores binary content in the database.
GUI-only preferences live in `weather-gui.toml` beside the selected engine
configuration (`~/.weather/config/weather-gui.toml` by default). The GUI debug
setting is disabled by default; it can be enabled from “关于与设置” to allow
selection, context menus, and F12 developer tools. Use `WEATHER_GUI_CONFIG` to
override the GUI configuration path.
The GUI also owns `weather-gui.db` in that directory for current-day display
snapshots only. It is not an engine cache: reopening the GUI and switching
stations always trigger a fresh engine update, and stale fallback is identified
by a persistent top-of-window warning. `WEATHER_GUI_DB` overrides this database
path.

### GUI dev build (Vite/HMR)

Use this mode while editing the frontend. Vite must remain running for the GUI
to load its resources:

```sh
cd weather-renderer/gui
bun install
bun run tauri dev --features desktop
```

The resulting `target/debug/weather-app` points to
`http://localhost:1420`. It is not standalone and shows a blank window when
launched without the Vite process started by `tauri dev`.

### GUI debug build with embedded assets

Use this mode to run a debug-profile GUI without Vite. The command builds the
frontend and embeds `dist` into the executable:

```sh
cd weather-renderer/gui
bun run standalone:debug
../../target/debug/weather.app
```

The executable can be started directly from another terminal. Its daemon code
is embedded and the GUI starts the same executable with the `daemon`
subcommand when an engine is needed.

Run `bun run bundle` from that directory for a native single-binary package. See
`weather-renderer/gui/README.md` for target and platform prerequisites.

## Service Installation

Service management currently supports systemd on Linux only. The `windows`
backend name is retained so unsupported SCM operations fail with a clear error;
`weather.app daemon` does not yet implement a Windows SCM service dispatcher.

Install the daemon as a user service:

```sh
weather.app daemon service install systemd
```

This installs files under `~/.weather/` by default, including:

- `~/.weather/bin/`
- `~/.weather/config/weather.toml`

Install as a system service:

```sh
sudo weather.app daemon service install systemd --system
```

System mode uses `/opt/weather` by default. You can override paths:

```sh
weather.app daemon service install systemd --path /custom/weather --config /custom/weather/weather.toml
```

Reinstall or remove the service:

```sh
weather.app daemon service reinstall systemd
weather.app daemon service remove systemd
weather.app daemon service remove systemd --all
```

Pass the same scope and path overrides when removing a custom installation:

```sh
weather.app daemon service remove systemd --system --path /custom/weather \
  --config /custom/weather/weather.toml --all
```

Use `--no-modification-service` when you want the installer to write files and
print next steps without starting or modifying the systemd service state. This
flag does not enable the unsupported Windows SCM backend; Windows service
commands fail before writing installation files.

## Build

Daemon-only debug build (the default feature set):

```sh
cargo build -p weather-app
```

Add `--features tui`, `--features gui`, or `--features desktop` to select the
compiled frontends. TUI and daemon are libraries and do not produce separate
release executables.

Commits pushed to `master` are bundled after the full CI workflow succeeds.
The desktop bundle workflow publishes Linux (Ubuntu 22.04-compatible), Windows
10, and macOS packages as the `weather-desktop-latest` GitHub Actions artifact.
Each package contains the single `weather.app` application binary with daemon,
TUI, and GUI support. Only the newest complete successful artifact set is
retained; a failed platform build leaves the preceding successful set intact.

Static release build:

```sh
make release-static
```

Release artifacts are copied to:

```text
target/release-artifacts/<target-triple>/
```

Cargo's original artifacts remain under
`target/<target-triple>/release-lto-static/`. Packaging verifies that each copy
matches its source, writes `SHA256SUMS`, and rejects dynamically linked Linux
binaries.

Recommended checks after code changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

## Updates

Refresh weather data manually:

```sh
weather.app tui once --refresh
```

Update configured stations through the frontend commands:

```sh
weather.app tui stations search "<query>"
weather.app tui stations add "<station>"
weather.app tui stations remove "<selector>"
weather.app tui stations enable "<selector>"
weather.app tui stations disable "<selector>"
```

Update installed binaries by rebuilding and reinstalling the service:

```sh
make release-static
target/release-artifacts/<target-triple>/weather.app daemon service reinstall systemd
```

## NMC Diagnostic Scripts

The executable scripts in `scripts/` make live NMC requests for upstream API
inspection and troubleshooting. Their arguments and defaults are:

| Script | Arguments | Defaults | Additional dependencies |
| --- | --- | --- | --- |
| `list_provinces.sh` | none | none | none |
| `list_cities.sh` | `[province_code]` | `ABJ` | none |
| `fetch_weather.sh` | `[station_id]` | `MjXfi` | none |
| `inspect_nmc_capabilities.sh` | `[station_id] [province_code]` | `Wqsps ABJ` | Python 3, `sed`, `mktemp`, `rm` |
| `explore_nmc_api.sh` | `[forecast_page_url] [station_id]` | `<base-url>/publish/forecast/ABJ/chaoyang.html MjXfi` | `sed`, `mktemp`, `rm` |

All five scripts require a POSIX-compatible `sh` and `curl`. Run them directly,
for example:

```sh
./scripts/list_provinces.sh
./scripts/list_cities.sh ABJ
./scripts/fetch_weather.sh MjXfi
./scripts/inspect_nmc_capabilities.sh Wqsps ABJ
./scripts/explore_nmc_api.sh \
  https://www.nmc.cn/publish/forecast/ABJ/chaoyang.html MjXfi
```

`NMC_BASE_URL` selects the upstream origin and defaults to
`https://www.nmc.cn`. Supply an origin without a trailing slash. It also
controls the default forecast page used by `explore_nmc_api.sh`; an explicit
first argument overrides that page URL.

```sh
NMC_BASE_URL=http://127.0.0.1:8080 ./scripts/fetch_weather.sh MjXfi
```
