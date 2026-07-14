#!/usr/bin/env sh
set -eu

base_url="${NMC_BASE_URL:-https://www.nmc.cn}"

curl -L --fail --show-error --silent \
  "${base_url}/rest/province/all"
