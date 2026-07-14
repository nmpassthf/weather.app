#!/usr/bin/env sh
set -eu

base_url="${NMC_BASE_URL:-https://www.nmc.cn}"
page_url="${1:-${base_url}/publish/forecast/ABJ/chaoyang.html}"
stationid="${2:-MjXfi}"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/weather-explore-nmc.XXXXXX")
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

printf '== forecast page ==\n'
forecast_page="${tmp_dir}/forecast.html"
curl -L --fail --show-error --silent --output "$forecast_page" "$page_url"
sed -n "s/.*var scode = '\([^']*\)'.*/scode: \1/p; s/.*CityWeather.getWeatherData('\([^']*\)', '\([^']*\)').*/weather call: stationid=\1 city=\2/p; s/.*name=stationId value=\([^ >]*\).*/stationId input: \1/p" \
  "$forecast_page"

printf '\n== weather json ==\n'
curl -L --fail --show-error --silent \
  "${base_url}/rest/weather?stationid=${stationid}"

printf '\n\n== provinces ==\n'
curl -L --fail --show-error --silent \
  "${base_url}/rest/province/all"

printf '\n'
