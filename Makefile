.PHONY: help run dev build release check test clippy fmt fmt-check clean doc doc-open \
        build-all build-core build-pal build-ui build-app deps cargo-update \
        test-all test-core test-pal test-ui test-app \
        run-release run-dev-trace get-size coverage profile lint-all \
        build-aarch64 deb-aarch64

CARGO := cargo
CORE_FEATURES := fs,sqlite,ics
TARGET_DIR := target
RELEASE_TARGET := $(TARGET_DIR)/release/ephemeris
DEBUG_TARGET := $(TARGET_DIR)/debug/ephemeris

# Help target - displays all available commands
help:
	@echo "Ephemeris Build & Dev System"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Dev & Run:"
	@echo "  run             - Run the app in debug mode (optimized dev build)"
	@echo "  dev             - Alias for 'run' - fast, with symbols for debugging"
	@echo "  run-release     - Run the optimized release binary (slower build, faster runtime)"
	@echo "  run-dev-trace   - Run with RUST_LOG=debug for tracing output"
	@echo ""
	@echo "Build:"
	@echo "  build           - Build debug binary"
	@echo "  build-release   - Build optimized release binary"
	@echo "  build-all       - Build all workspace crates (debug)"
	@echo "  build-core      - Build ephemeris-core only"
	@echo "  build-pal       - Build ephemeris-pal only"
	@echo "  build-ui        - Build ephemeris-ui only"
	@echo "  build-app       - Build ephemeris-app binary"
	@echo ""
	@echo "Check & Lint:"
	@echo "  check           - Run cargo check (fast validation without build)"
	@echo "  clippy          - Run clippy linter with all warnings"
	@echo "  lint-all        - Run check + clippy + fmt-check"
	@echo ""
	@echo "Test:"
	@echo "  test            - Run all tests (all crates)"
	@echo "  test-all        - Run all tests with backtrace"
	@echo "  test-core       - Test ephemeris-core only"
	@echo "  test-pal        - Test ephemeris-pal only"
	@echo "  test-ui         - Test ephemeris-ui only"
	@echo "  test-app        - Test ephemeris-app only"
	@echo "  coverage        - Run tests with coverage (requires tarpaulin)"
	@echo ""
	@echo "Code Quality:"
	@echo "  fmt             - Format code with rustfmt"
	@echo "  fmt-check       - Check if code is formatted (no changes)"
	@echo ""
	@echo "Documentation:"
	@echo "  doc             - Build documentation (no deps)"
	@echo "  doc-open        - Build and open documentation in browser"
	@echo ""
	@echo "Packaging (PineNote / aarch64):"
	@echo "  build-aarch64   - Cross-build release binary for aarch64-linux"
	@echo "  deb-aarch64     - Build arm64 .deb from aarch64 release binary"
	@echo ""
	@echo "Maintenance:"
	@echo "  clean           - Remove build artifacts"
	@echo "  deps            - Check for outdated dependencies"
	@echo "  cargo-update    - Update Cargo.lock to latest compatible versions"
	@echo "  get-size        - Show size of release binary"
	@echo "  profile         - Build release with profiling info for flamegraph"
	@echo ""

# Run targets
run: build
	@echo "🚀 Running Ephemeris (debug mode)..."
	@$(DEBUG_TARGET)

dev: run

run-release: build-release
	@echo "🚀 Running Ephemeris (release optimized)..."
	@$(RELEASE_TARGET)

run-dev-trace: build
	@echo "🚀 Running Ephemeris with debug tracing..."
	@RUST_LOG=debug $(DEBUG_TARGET)

# Build targets
build:
	@echo "🔨 Building ephemeris (debug)..."
	@$(CARGO) build --package ephemeris-app

build-release:
	@echo "🔨 Building ephemeris (release optimized)..."
	@$(CARGO) build --release --package ephemeris-app

build-all:
	@echo "🔨 Building all workspace crates (debug)..."
	@$(CARGO) build --workspace

build-core:
	@echo "🔨 Building ephemeris-core..."
	@$(CARGO) build --package ephemeris-core --features $(CORE_FEATURES)

build-pal:
	@echo "🔨 Building ephemeris-pal..."
	@$(CARGO) build --package ephemeris-pal

build-ui:
	@echo "🔨 Building ephemeris-ui..."
	@$(CARGO) build --package ephemeris-ui

build-app:
	@echo "🔨 Building ephemeris-app..."
	@$(CARGO) build --package ephemeris-app

# Check & Lint
check:
	@echo "✅ Checking code (cargo check)..."
	@$(CARGO) check --workspace --all-targets

clippy:
	@echo "🔍 Running clippy linter..."
	@$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	@echo "📝 Formatting code with rustfmt..."
	@$(CARGO) fmt --all

fmt-check:
	@echo "📝 Checking code format..."
	@$(CARGO) fmt --all -- --check

lint-all: check clippy fmt-check
	@echo "✅ All lint checks passed!"

# Test targets
test:
	@echo "🧪 Running tests..."
	@$(CARGO) test --workspace

test-all:
	@echo "🧪 Running all tests with backtrace..."
	@RUST_BACKTRACE=1 $(CARGO) test --workspace

test-core:
	@echo "🧪 Testing ephemeris-core..."
	@$(CARGO) test --package ephemeris-core --features $(CORE_FEATURES)

test-pal:
	@echo "🧪 Testing ephemeris-pal..."
	@$(CARGO) test --package ephemeris-pal

test-ui:
	@echo "🧪 Testing ephemeris-ui..."
	@$(CARGO) test --package ephemeris-ui

test-app:
	@echo "🧪 Testing ephemeris-app..."
	@$(CARGO) test --package ephemeris-app

coverage:
	@echo "📊 Running coverage analysis (requires cargo-tarpaulin)..."
	@$(CARGO) tarpaulin --workspace --out Html --output-dir coverage

# Code quality
fmt: _fmt

_fmt:
	@$(CARGO) fmt --all

# Documentation
doc:
	@echo "📚 Building documentation..."
	@$(CARGO) doc --workspace --no-deps

doc-open:
	@echo "📚 Building and opening documentation..."
	@$(CARGO) doc --workspace --no-deps --open

# Maintenance
clean:
	@echo "🧹 Cleaning build artifacts..."
	@$(CARGO) clean
	@rm -rf coverage/

deps:
	@echo "📦 Checking for outdated dependencies..."
	@$(CARGO) outdated

cargo-update:
	@echo "📦 Updating Cargo.lock..."
	@$(CARGO) update

get-size:
	@echo "📏 Release binary size:"
	@ls -lh $(RELEASE_TARGET) 2>/dev/null || echo "   (build with 'make build-release' first)"
	@du -sh $(TARGET_DIR) 2>/dev/null || echo "   (target directory size unavailable)"

profile:
	@echo "🔬 Building release with profiling info..."
	@RUSTFLAGS="-g" $(CARGO) build --release --package ephemeris-app
	@echo "   Built at: $(RELEASE_TARGET)"
	@echo "   Use with: flamegraph --bin ephemeris"

# Aliases
.PHONY: build-release _fmt

# Packaging for PineNote (aarch64 / arm64 .deb)
AARCH64_TARGET := aarch64-unknown-linux-gnu
AARCH64_BIN := $(TARGET_DIR)/$(AARCH64_TARGET)/release/ephemeris
VERSION ?= $(shell sed -n '/\[workspace.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST_DIR := dist

build-aarch64:
	@echo "🔨 Cross-building ephemeris for $(AARCH64_TARGET)..."
	@cross build --target $(AARCH64_TARGET) -p ephemeris-app --release

deb-aarch64: build-aarch64
	@echo "📦 Building arm64 .deb (version $(VERSION))..."
	@mkdir -p $(DIST_DIR)
	@chmod +x scripts/build-deb.sh
	@scripts/build-deb.sh $(AARCH64_BIN) $(VERSION) $(DIST_DIR)
