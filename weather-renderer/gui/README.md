# Weather GUI

This renderer is a Tauri 2 desktop frontend for the same local
`weather-daemon` / `weather-engine` API used by `weather-tui`. It does not read
the database, write the engine TOML directly, or call the NMC provider; only
its separate renderer-owned `weather-gui.toml` is persisted locally.

## Features

- current conditions, forecast, warning, air quality, history, climate, radar,
  stale-data state, and engine event log;
- ordered multi-alert display with current-station alerts followed by
  deduplicated two-level parent alerts for three-level stations;
- interactive temperature history with desktop hover, mobile long-press/drag,
  and keyboard point inspection;
- consistent multi-day labels across forecasts and charts: yesterday, today,
  tomorrow, weekdays through day +7, then concrete calendar dates;
- a horizontally scrollable daily high/low chart that combines every stored
  engine-DB history date with the latest forecast, opens on a responsive
  two-history-day through seven-forecast-day window, and supports bidirectional
  scrolling plus the same hover, long-press/drag, and keyboard inspection;
  older DB days are fetched in stable date-cursor pages when the user
  reaches the loaded past boundary;
- an in-window large-image viewer for radar resources with wheel/button zoom,
  image panning, and a draggable viewer panel;
- station search and weather preview;
- station add, remove, enable/disable, ordering, and GUI-session-only hiding;
- current/default config inspection plus engine status, restart, and stop;
- a GUI-owned current-day SQLite display cache for reopen and station switching;
- persistent GUI-only settings with a guarded debug/inspector mode;
- local fallback assets, light/dark themes, keyboard focus, reduced-motion, and
  responsive layouts for Windows, macOS, and Linux desktop sizes.

## Develop

The package manager is pinned to Bun 1.3.14 in `package.json`. Build the daemon
once, install frontend dependencies, then launch Tauri:

```sh
cargo build -p weather-daemon
cd weather-renderer/gui
bun install
bun run tauri dev
```

The GUI finds `weather-daemon` beside the app, in `PATH`, or through
`WEATHER_DAEMON_EXE`. `WEATHER_CONFIG` selects a non-default config path.
When no daemon is running, the GUI starts an owner-token-bound foreground
daemon and shuts down only that owned process when the main window or GUI
process exits. A daemon that was already running before GUI startup is adopted
for the session and is left running on exit, matching the TUI lifecycle.
GUI-only settings are stored separately in `weather-gui.toml`, beside the
resolved engine config file (`~/.weather/config/weather-gui.toml` by default).
`WEATHER_GUI_CONFIG` overrides that path. The `debug` option defaults to
`false`; while disabled, the WebView inspector, context menu, element
selection, and developer shortcuts are unavailable. Enabling it under
“关于与设置” persists the option and restarts the GUI so F12 and the native
inspector can be used.
The separate `weather-gui.db` file is stored in the same directory by default;
`WEATHER_GUI_DB` overrides its path. It keeps at most one current-local-day
display snapshot per configured station, removes older-day/unconfigured rows
on access, and excludes debug payloads and process-local image resource IDs.
Cached snapshots are marked stale and are used only to avoid an empty screen
while reopening the GUI or switching stations. Every GUI launch and station
switch still forces an engine refresh. Cold-engine refreshes use the provider
network timeout budget rather than the short control-RPC timeout. A failed
refresh, including an engine
response that falls back to stale data, leaves a persistent warning at the top
until a fresh update succeeds.
The engine also treats a configured three-level station's two-level parent as
implicitly tracked. Parent refreshes run before child refreshes, are persisted
to the engine DB, and use the configured weather TTL (600 seconds by default
for new configurations), but implicit parents are not added to the GUI station
list or user config.
For a direct connection, set both `WEATHER_RPC_ENDPOINT` and
`WEATHER_PUB_ENDPOINT`. HMAC-enabled engines can use `WEATHER_HMAC_KEY`, or
`WEATHER_HMAC_ENV_KEY_NAME` to name the environment variable containing the
key.

## Verify and package

```sh
bun install --frozen-lockfile
bun run build
cargo check -p weather-gui
bun run bundle
```

`bun run bundle` stages a release `weather-daemon` into the app resources and
then runs the native Tauri packager. Set `WEATHER_CARGO_TARGET` when staging a
specific Rust target triple. Native packaging still requires the platform's
normal Tauri prerequisites (WebView2 on Windows and WebKitGTK development
packages on Linux).

All bundled image sources are managed under `assets/`; see `assets/README.md`
before replacing placeholders. The WebView never requests NMC directly:
remote resource URLs are converted to opaque IDs by `weather-engine`, fetched
through an asynchronous engine transfer, and delivered to Tauri as raw bytes.
The first request starts the provider download and returns `PENDING` without
holding the normal RPC timeout; the Tauri bridge polls until `READY`, then
assembles bounded 512 KiB offset-based chunks. Large resources use a bounded
32 MiB in-memory engine cache with a 15-minute TTL; resource bytes are never
written to SQLite or another database.
