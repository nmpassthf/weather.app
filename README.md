# Weather App

A layered Rust weather client and local weather service. The current frontend is
`weather-tui`, backed by a local `weather-engine` managed by `weather-daemon`.
Weather data is fetched from the NMC provider, cached locally, and exposed to
frontends through the shared `weather-schema` protocol.

TUI screenshots:

*Main screen*

![main screenshot](https://raw.githubusercontent.com/nmpassthf/weather.app/refs/heads/master/docs/README/main.png)

*Search menu*

![search screenshot](https://raw.githubusercontent.com/nmpassthf/weather.app/refs/heads/master/docs/README/search.png)

## Usage

Build the binaries first, then run the TUI:

```sh
cargo build --workspace --bins
./target/debug/weather-tui
```

Common commands:

```sh
# One-shot weather output
weather-tui once

# JSON output
weather-tui --format json once

# Force a fresh weather fetch
weather-tui once --refresh

# Search for a station
weather-tui search "北京"

# Add or update the configured station through engine APIs
weather-tui search "北京" --write
weather-tui stations list
weather-tui stations add "北京-北京市"

# Show engine status or stop the active engine
weather-tui status
weather-tui kill
```

Use `-c/--config <path>` to select a config file. By default, the app uses
`~/.weather/config/weather.toml`. Frontends should request and submit structured
config changes through the engine; they should not edit the TOML file directly
during normal operation.

Useful config commands:

```sh
weather-tui --core-get-default-config
weather-tui --core-get-config
weather-tui --core-restart-engine
```

## Service Installation

Install the daemon as a user service:

```sh
weather-daemon service install systemd
```

This installs files under `~/.weather/` by default, including:

- `~/.weather/bin/`
- `~/.weather/config/weather.toml`

Install as a system service:

```sh
sudo weather-daemon service install systemd --system
```

System mode uses `/opt/weather` by default. You can override paths:

```sh
weather-daemon service install systemd --path /custom/weather --config /custom/weather/weather.toml
```

Reinstall or remove the service:

```sh
weather-daemon service reinstall systemd
weather-daemon service remove systemd
weather-daemon service remove systemd --all
```

Use `--no-modification-service` when you want the installer to write files and
print next steps without starting or modifying the service state.

## Build

Debug build:

```sh
cargo build --workspace --bins
```

Static release build:

```sh
make release-static
```

Release artifacts are copied to:

```text
target/release-lto-static/
```

Recommended checks after code changes:

```sh
cargo fmt
cargo test --workspace
cargo clippy --all-targets --all-features
```

## Updates

Refresh weather data manually:

```sh
weather-tui once --refresh
```

Update configured stations through the frontend commands:

```sh
weather-tui search "<query>" --write
weather-tui stations add "<station>"
weather-tui stations remove "<selector>"
weather-tui stations enable "<selector>"
weather-tui stations disable "<selector>"
```

Update installed binaries by rebuilding and reinstalling the service:

```sh
make release-static
target/release-lto-static/weather-daemon service reinstall systemd
```

For upstream API inspection or troubleshooting, use the scripts in `scripts/`.
