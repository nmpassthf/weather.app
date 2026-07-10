#!/usr/bin/env sh
set -eu

province_code="${1:-ABJ}"
base_url="${NMC_BASE_URL:-https://www.nmc.cn}"

curl -L --fail --show-error --silent \
  "${base_url}/rest/province/${province_code}"
