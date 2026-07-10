#!/usr/bin/env sh
set -eu

base_url="${NMC_BASE_URL:-https://www.nmc.cn}"
stationid="${1:-Wqsps}"
province_code="${2:-ABJ}"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/weather-inspect-nmc.XXXXXX")
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

fetch() {
  label="$1"
  url="$2"
  printf '\n== %s ==\n' "$label"
  response_file="${tmp_dir}/response.json"
  formatted_file="${tmp_dir}/formatted.json"
  curl -L --fail --show-error --silent --output "$response_file" "$url"
  python3 -m json.tool "$response_file" >"$formatted_file"
  sed -n '1,220p' "$formatted_file"
}

fetch "weather station=${stationid}" "${base_url}/rest/weather?stationid=${stationid}"
fetch "position station=${stationid}" "${base_url}/rest/position?stationid=${stationid}"
fetch "province all" "${base_url}/rest/province/all"
fetch "province ${province_code}" "${base_url}/rest/province/${province_code}"
