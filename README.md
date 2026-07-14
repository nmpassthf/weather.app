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
weather-tui stations search "北京"

# Add or update the configured station through engine APIs
weather-tui stations list
weather-tui stations add "北京-北京市"

# Show engine status or stop the active engine
weather-tui engine status
weather-tui engine stop
```

Use `-c/--config <path>` to select a config file. By default, the app uses
`~/.weather/config/weather.toml`. Frontends should request and submit structured
config changes through the engine; they should not edit the TOML file directly
during normal operation.

Useful config commands:

```sh
weather-tui config defaults
weather-tui config show
weather-tui engine restart
```

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

## Service Installation

Service management currently supports systemd on Linux only. The `windows`
backend name is retained so unsupported SCM operations fail with a clear error;
`weather-daemon` does not yet implement a Windows SCM service dispatcher.

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

Pass the same scope and path overrides when removing a custom installation:

```sh
weather-daemon service remove systemd --system --path /custom/weather \
  --config /custom/weather/weather.toml --all
```

Use `--no-modification-service` when you want the installer to write files and
print next steps without starting or modifying the systemd service state. This
flag does not enable the unsupported Windows SCM backend; Windows service
commands fail before writing installation files.

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
weather-tui once --refresh
```

Update configured stations through the frontend commands:

```sh
weather-tui stations search "<query>"
weather-tui stations add "<station>"
weather-tui stations remove "<selector>"
weather-tui stations enable "<selector>"
weather-tui stations disable "<selector>"
```

Update installed binaries by rebuilding and reinstalling the service:

```sh
make release-static
target/release-artifacts/<target-triple>/weather-daemon service reinstall systemd
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
