STATIC_TARGET ?= $(shell rustc -vV | awk '/^host:/ { print $$2 }')
STATIC_TARGET_ENV := $(shell printf '%s' '$(STATIC_TARGET)' | tr '[:lower:]-' '[:upper:]_')
CARGO_ARGS ?=
APP_FEATURES ?= daemon
TARGET_RELEASE_STATIC_DIR := target/$(STATIC_TARGET)/release-lto-static
RELEASE_ARTIFACT_DIR := target/release-artifacts/$(STATIC_TARGET)
BIN_SUFFIX := $(if $(findstring windows,$(STATIC_TARGET)),.exe,)
SOURCE_ARTIFACT := weather-app$(BIN_SUFFIX)
PUBLISHED_ARTIFACT := weather.app$(BIN_SUFFIX)
ARTIFACTS := $(PUBLISHED_ARTIFACT)
SHA256SUM ?= sha256sum
SHA256SUM_ARGS ?=
READELF ?= readelf

.PHONY: all release-static clean

release-static:
	env CARGO_TARGET_$(STATIC_TARGET_ENV)_RUSTFLAGS="$${CARGO_TARGET_$(STATIC_TARGET_ENV)_RUSTFLAGS:-} -C target-feature=+crt-static" \
		cargo build -p weather-app --no-default-features --features "$(APP_FEATURES)" \
			--profile release-lto-static --target "$(STATIC_TARGET)" $(CARGO_ARGS)
	set -eu; \
	mkdir -p "$(RELEASE_ARTIFACT_DIR)"; \
	for artifact in $(ARTIFACTS); do \
		src="$(TARGET_RELEASE_STATIC_DIR)/$(SOURCE_ARTIFACT)"; \
		dst="$(RELEASE_ARTIFACT_DIR)/$$artifact"; \
		if [ ! -f "$$src" ]; then \
			echo "missing build artifact: $$src" >&2; \
			exit 1; \
		fi; \
		if ! cp -fp "$$src" "$$dst"; then \
			echo "failed to copy build artifact: $$src" >&2; \
			exit 1; \
		fi; \
		if [ ! -f "$$src" ] || ! cmp -s "$$src" "$$dst"; then \
			echo "copied artifact does not match its source: $$artifact" >&2; \
			exit 1; \
		fi; \
	done; \
	case "$(STATIC_TARGET)" in \
		*-linux-*) \
			command -v "$(READELF)" >/dev/null 2>&1 || { echo "missing ELF inspector: $(READELF)" >&2; exit 1; }; \
			for artifact in $(ARTIFACTS); do \
				artifact="$(RELEASE_ARTIFACT_DIR)/$$artifact"; \
				if ! program_headers=$$("$(READELF)" -lW "$$artifact"); then \
					echo "failed to inspect program headers: $$artifact" >&2; \
					exit 1; \
				fi; \
				case "$$program_headers" in *INTERP*) echo "dynamic interpreter found in $$artifact" >&2; exit 1;; esac; \
				if ! dynamic_section=$$("$(READELF)" -dW "$$artifact"); then \
					echo "failed to inspect dynamic entries: $$artifact" >&2; \
					exit 1; \
				fi; \
				case "$$dynamic_section" in *NEEDED*) echo "dynamic dependency found in $$artifact" >&2; exit 1;; esac; \
			done \
		;; \
	esac; \
	command -v "$(SHA256SUM)" >/dev/null 2>&1 || { echo "missing checksum tool: $(SHA256SUM)" >&2; exit 1; }; \
	cd "$(RELEASE_ARTIFACT_DIR)"; \
	if ! "$(SHA256SUM)" $(SHA256SUM_ARGS) $(ARTIFACTS) > SHA256SUMS; then \
		echo "failed to generate SHA256SUMS" >&2; \
		exit 1; \
	fi; \
	"$(SHA256SUM)" $(SHA256SUM_ARGS) -c SHA256SUMS

all: release-static

clean:
	cargo clean
