.DEFAULT_GOAL := all

NATIVE_TARGET ?= $(shell rustc -vV | awk '/^host:/ { print $$2 }')
MUSL_TARGET ?= x86_64-unknown-linux-musl
MUSL_TARGET_ENV := $(subst -,_,$(MUSL_TARGET))
RELEASE_PROFILE ?= release-lto-static
CARGO_ARGS ?=
BUN ?= bun

NATIVE_ALL_FEATURES ?= daemon,tui,gui
NATIVE_GUI_FEATURES ?= daemon,gui
NATIVE_TUI_FEATURES ?= daemon,tui
MUSL_TUI_FEATURES ?= daemon,tui

GUI_DIR := weather-renderer/gui
RELEASE_ARTIFACT_ROOT := target/release-artifacts
MUSL_CC ?= $(shell command -v x86_64-linux-musl-gcc 2>/dev/null || command -v musl-gcc 2>/dev/null)
SHA256SUM ?= $(shell command -v sha256sum 2>/dev/null || command -v shasum 2>/dev/null)
SHA256SUM_ARGS ?= $(if $(findstring shasum,$(notdir $(SHA256SUM))),-a 256,)
READELF ?= $(shell command -v readelf 2>/dev/null || command -v llvm-readelf 2>/dev/null)

bin_suffix = $(if $(findstring windows,$(1)),.exe,)
source_artifact = target/$(1)/$(RELEASE_PROFILE)/weather-app$(call bin_suffix,$(1))
published_artifact = weather.app$(call bin_suffix,$(1))
artifact_dir = $(RELEASE_ARTIFACT_ROOT)/$(1)/$(2)

define publish_artifact
set -eu; \
src="$(call source_artifact,$(1))"; \
dst_dir="$(call artifact_dir,$(1),$(2))"; \
dst="$$dst_dir/$(call published_artifact,$(1))"; \
test -f "$$src" || { echo "missing build artifact: $$src" >&2; exit 1; }; \
test -n "$(SHA256SUM)" || { echo "missing SHA-256 tool: install sha256sum or shasum" >&2; exit 1; }; \
mkdir -p "$$dst_dir"; \
cp -fp "$$src" "$$dst"; \
cmp -s "$$src" "$$dst" || { echo "copied artifact does not match its source: $$dst" >&2; exit 1; }; \
cd "$$dst_dir"; \
"$(SHA256SUM)" $(SHA256SUM_ARGS) "$(call published_artifact,$(1))" > SHA256SUMS; \
"$(SHA256SUM)" $(SHA256SUM_ARGS) -c SHA256SUMS
endef

define verify_static_elf
set -eu; \
artifact="$(call source_artifact,$(1))"; \
test -n "$(READELF)" || { echo "missing ELF inspector: install readelf or llvm-readelf" >&2; exit 1; }; \
program_headers="$$("$(READELF)" -lW "$$artifact")"; \
case "$$program_headers" in *INTERP*) echo "dynamic interpreter found in $$artifact" >&2; exit 1;; esac; \
dynamic_section="$$("$(READELF)" -dW "$$artifact")"; \
case "$$dynamic_section" in *NEEDED*) echo "dynamic dependency found in $$artifact" >&2; exit 1;; esac
endef

.PHONY: all gui tui native-all native-gui native-tui musl-tui \
	frontend-assets release-static help clean

all: native-all

gui: native-gui

tui: native-tui

native-all: frontend-assets
	cargo build -p weather-app --no-default-features \
		--features "$(NATIVE_ALL_FEATURES)" --profile "$(RELEASE_PROFILE)" \
		--target "$(NATIVE_TARGET)" $(CARGO_ARGS)
	@$(call publish_artifact,$(NATIVE_TARGET),all)

native-gui: frontend-assets
	cargo build -p weather-app --no-default-features \
		--features "$(NATIVE_GUI_FEATURES)" --profile "$(RELEASE_PROFILE)" \
		--target "$(NATIVE_TARGET)" $(CARGO_ARGS)
	@$(call publish_artifact,$(NATIVE_TARGET),gui)

native-tui:
	cargo build -p weather-app --no-default-features \
		--features "$(NATIVE_TUI_FEATURES)" --profile "$(RELEASE_PROFILE)" \
		--target "$(NATIVE_TARGET)" $(CARGO_ARGS)
	@$(call publish_artifact,$(NATIVE_TARGET),tui)

musl-tui:
	@test -n "$(MUSL_CC)" || { \
		echo "missing musl C compiler; install musl-tools or set MUSL_CC" >&2; \
		exit 1; \
	}
	@rustup target list --installed | grep -qx "$(MUSL_TARGET)" || { \
		echo "missing Rust target: run rustup target add $(MUSL_TARGET)" >&2; \
		exit 1; \
	}
	env CC_$(MUSL_TARGET_ENV)="$(MUSL_CC)" \
		cargo build -p weather-app --no-default-features \
			--features "$(MUSL_TUI_FEATURES)" --profile "$(RELEASE_PROFILE)" \
			--target "$(MUSL_TARGET)" $(CARGO_ARGS)
	@$(call verify_static_elf,$(MUSL_TARGET))
	@$(call publish_artifact,$(MUSL_TARGET),musl-tui)

frontend-assets:
	@command -v "$(BUN)" >/dev/null 2>&1 || { echo "missing Bun: $(BUN)" >&2; exit 1; }
	cd "$(GUI_DIR)" && "$(BUN)" install --frozen-lockfile
	cd "$(GUI_DIR)" && "$(BUN)" run build

# Backward-compatible name for the static terminal release.
release-static: musl-tui

help:
	@echo "make              Native daemon + TUI + GUI (default)"
	@echo "make gui          Native daemon + GUI"
	@echo "make tui          Native daemon + TUI"
	@echo "make musl-tui     Static musl daemon + TUI"
	@echo "make clean        Remove Cargo build artifacts"

clean:
	cargo clean
