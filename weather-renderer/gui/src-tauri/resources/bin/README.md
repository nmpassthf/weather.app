# Bundled daemon slot

`bun run bundle` builds `weather-daemon` and stages the platform executable in
this directory before Tauri packages the app. The executable is ignored by Git.

For a non-host Cargo target, set `WEATHER_CARGO_TARGET` to its Rust target
triple before running the bundle command.
