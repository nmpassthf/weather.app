STATIC_TARGET ?= $(shell rustc -vV | awk '/^host:/ { print $$2 }')
STATIC_TARGET_ENV := $(shell printf '%s' '$(STATIC_TARGET)' | tr '[:lower:]-' '[:upper:]_')
CARGO_ARGS ?=
RELEASE_STATIC_DIR := target/release-lto-static
TARGET_RELEASE_STATIC_DIR := target/$(STATIC_TARGET)/release-lto-static
BINS := weather-tui weather-daemon

.PHONY: all release-static clean

release-static:
	env CARGO_TARGET_$(STATIC_TARGET_ENV)_RUSTFLAGS="$${CARGO_TARGET_$(STATIC_TARGET_ENV)_RUSTFLAGS:-} -C target-feature=+crt-static" \
		cargo build --workspace --bins --profile release-lto-static --target "$(STATIC_TARGET)" $(CARGO_ARGS)
	mkdir -p "$(RELEASE_STATIC_DIR)"
	for bin in $(BINS); do \
		for artifact in "$$bin" "$$bin.d"; do \
			src="$(TARGET_RELEASE_STATIC_DIR)/$$artifact"; \
			dst="$(RELEASE_STATIC_DIR)/$$artifact"; \
			if [ -e "$$src" ]; then \
				if [ -e "$$dst" ] && [ "$$src" -ef "$$dst" ]; then \
					:; \
				else \
					mv -f "$$src" "$$dst"; \
				fi; \
			elif [ ! -e "$$dst" ]; then \
				echo "missing build artifact: $$src" >&2; \
				exit 1; \
			fi; \
		done; \
	done

all: release-static

clean:
	cargo clean
