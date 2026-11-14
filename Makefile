# Linux source installation. No sudo or publication; Cargo may fetch locked dependencies.
PREFIX ?= $(HOME)/.local
DESTDIR ?=
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
MANDIR ?= $(DATADIR)/man
CARGO ?= cargo
CARGO_FEATURES ?=
TARGET_DIR ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)
BIN ?= $(TARGET_DIR)/release/ztail
ASSET_DIR ?= $(TARGET_DIR)/generated

.PHONY: build assets install uninstall
build:
	$(CARGO) build --locked --release $(CARGO_FEATURES)

assets: build
	@set -eu; mkdir -p "$(ASSET_DIR)"; \
	for shell in bash zsh fish elvish powershell; do \
	  "$(BIN)" --language=en --color=never --completions "$$shell" > "$(ASSET_DIR)/$$shell"; \
	done; \
	"$(BIN)" --language=en --color=never --manpage > "$(ASSET_DIR)/ztail.1"; \
	"$(BIN)" --build-info > "$(ASSET_DIR)/BUILD-INFO.json"

install: assets
	install -Dm755 "$(BIN)" "$(DESTDIR)$(BINDIR)/ztail"
	install -Dm644 "$(ASSET_DIR)/ztail.1" "$(DESTDIR)$(MANDIR)/man1/ztail.1"
	install -Dm644 "$(ASSET_DIR)/bash" "$(DESTDIR)$(DATADIR)/bash-completion/completions/ztail"
	install -Dm644 "$(ASSET_DIR)/zsh" "$(DESTDIR)$(DATADIR)/zsh/site-functions/_ztail"
	install -Dm644 "$(ASSET_DIR)/fish" "$(DESTDIR)$(DATADIR)/fish/vendor_completions.d/ztail.fish"
	install -Dm644 "$(ASSET_DIR)/elvish" "$(DESTDIR)$(DATADIR)/ztail/completions/ztail.elv"
	install -Dm644 "$(ASSET_DIR)/powershell" "$(DESTDIR)$(DATADIR)/ztail/completions/ztail.ps1"
	install -Dm644 "$(ASSET_DIR)/BUILD-INFO.json" "$(DESTDIR)$(DATADIR)/ztail/BUILD-INFO.json"

# Remove only the files owned by this install target, never entire directories.
uninstall:
	rm -f -- "$(DESTDIR)$(BINDIR)/ztail" "$(DESTDIR)$(MANDIR)/man1/ztail.1" \
	  "$(DESTDIR)$(DATADIR)/bash-completion/completions/ztail" \
	  "$(DESTDIR)$(DATADIR)/zsh/site-functions/_ztail" \
	  "$(DESTDIR)$(DATADIR)/fish/vendor_completions.d/ztail.fish" \
	  "$(DESTDIR)$(DATADIR)/ztail/completions/ztail.elv" \
	  "$(DESTDIR)$(DATADIR)/ztail/completions/ztail.ps1" \
	  "$(DESTDIR)$(DATADIR)/ztail/BUILD-INFO.json"
