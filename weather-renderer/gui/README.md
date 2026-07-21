# Weather GUI

The Weather GUI is the Tauri 2 desktop interface embedded in the single
`weather.app` executable. It communicates with the same local engine as the
TUI and does not access the provider or engine database directly.

## Features

- Current conditions, forecasts, alerts, air quality, climate, and radar.
- Station search, preview, ordering, and enable/disable management.
- Scrollable temperature history with mouse, touch, and keyboard inspection.
- Local display cache with stale-data fallback and background refresh.
- Engine status, event log, restart, and stop controls.
- Light/dark themes, responsive layout, reduced motion, and keyboard access.

## Prerequisites

Install stable Rust and Bun 1.3.14. Native packaging also requires the Tauri
dependencies for the current platform.

Ubuntu 22.04:

```sh
sudo apt-get update
sudo apt-get install --yes \
  build-essential pkg-config libglib2.0-dev libgtk-3-dev \
  libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf libfuse2
```

Windows 10 uses the MSVC Rust toolchain, Windows SDK, and WebView2. macOS uses
the Xcode command-line tools; release bundles target macOS 11 or newer.

## Development

Install dependencies and start Tauri with Vite hot reload:

```sh
cd weather-renderer/gui
bun install --frozen-lockfile
bun run tauri dev
```

The command automatically compiles the `desktop` feature set and explicitly
starts GUI mode. The resulting development executable depends on the Vite
server and should be launched through this command.

Run frontend checks:

```sh
bun test
bun run build
```

Build a standalone debug executable with embedded frontend assets:

```sh
bun run standalone:debug
../../target/debug/weather.app gui
```

GUI settings and the current-day display cache are stored beside the engine
configuration under `~/.weather/config/`. `WEATHER_CONFIG`,
`WEATHER_GUI_CONFIG`, and `WEATHER_GUI_DB` override their default paths.

## Package

Build the native bundle for the current platform:

```sh
bun run bundle
```

Tauri writes installers and application bundles under
`target/release/bundle/`. Each package contains one `weather.app` application
binary with daemon, TUI, and GUI support; no daemon sidecar is required.

The CI release targets are Ubuntu 22.04, Windows 10, and universal macOS.
Platform builds produce AppImage/deb, NSIS, and app/dmg packages respectively.

Bundled image sources live under `assets/`. Read
[assets/README.md](assets/README.md) before replacing generated or placeholder
assets.
