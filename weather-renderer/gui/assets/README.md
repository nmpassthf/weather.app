# GUI assets

All frontend image resources live here and are referenced only through
`src/assets.ts`. This keeps paths stable when Vite fingerprints production
assets.

- `brand/`: product mark and the vector source for bundle icons.
- `weather/`: local weather-condition fallbacks. Provider icon URLs are never
  required for the UI to render.
- `icons/`: small semantic UI icons used to label weather measurements.
- `illustrations/`: empty, unavailable, remote-image fallback states, and the
  local radar resource used by browser demo mode.

The current SVG files are project-owned placeholders. Replace a file in place
to preserve imports. Keep a `viewBox`, meaningful contrast in light and dark
themes, and avoid embedded external resources or scripts.

Tauri bundle icons under `src-tauri/icons/` are generated from
`brand/app-icon.svg`; they are packaging resources rather than frontend assets.
