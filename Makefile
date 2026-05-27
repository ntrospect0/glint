# glint — terminal dashboard for stocks, calendar, news, and beyond.
#
# Build + install the `glint` binary. Override PREFIX to redirect the
# install destination — `/usr/local/bin` by default (system-wide,
# typically needs sudo), or `~/.local/bin` for a no-sudo per-user install.
#
# Common recipes:
#   make build         # debug build at target/debug/glint
#   make release       # release build at target/release/glint
#   make install       # build + copy to $(PREFIX)/bin/glint
#   make uninstall     # remove $(PREFIX)/bin/glint
#   make clean         # cargo clean
#   make test          # run the test suite
#
# Per-user install (no sudo needed):
#   make install PREFIX=~/.local
#
# System-wide install:
#   sudo make install

PREFIX ?= /usr/local
BINDIR := $(PREFIX)/bin
BIN := glint
TARGET := target/release/$(BIN)
SRC := $(shell find src -type f -name '*.rs') Cargo.toml Cargo.lock

# Default target builds a release binary so `make && make install` works.
.PHONY: all
all: release

.PHONY: build
build:
	cargo build

.PHONY: release
release: $(TARGET)

$(TARGET): $(SRC)
	cargo build --release

.PHONY: test
test:
	cargo test --quiet

.PHONY: install
install: release
	@mkdir -p $(BINDIR)
	install -m 755 $(TARGET) $(BINDIR)/$(BIN)
	@echo "Installed $(BINDIR)/$(BIN)"
	@echo
	@echo "If $(BINDIR) isn't on your \$$PATH yet, add it:"
	@echo "  echo 'export PATH=\"$(BINDIR):\$$PATH\"' >> ~/.zshrc   # or ~/.bashrc"
	@echo
	@echo "Then run \`glint\` to launch. First run drops you into the setup wizard."

.PHONY: uninstall
uninstall:
	rm -f $(BINDIR)/$(BIN)
	@echo "Removed $(BINDIR)/$(BIN)"

.PHONY: clean
clean:
	cargo clean

.PHONY: version
version:
	@$(TARGET) --version 2>/dev/null || cargo run --release --quiet -- --version
