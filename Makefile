# herdr-wakeup - build and install the Herdr watcher binary.
#
#   make               # build release binary
#   make install       # build + copy wakeup-herdr to $(PREFIX)/bin
#   make plugin-link   # link this plugin into Herdr for local development
#
# The standalone `wakeup` binary must be installed separately and available on PATH.

PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
BINS := wakeup-herdr

.PHONY: all build install uninstall plugin-link plugin-unlink clean

all: build

build:
	cargo build --release

install: build
	@mkdir -p "$(BINDIR)"
	@for b in $(BINS); do \
		install -m 0755 "target/release/$$b" "$(BINDIR)/$$b" && \
		echo "installed: $(BINDIR)/$$b"; \
	done
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; \
		*) echo "note: add $(BINDIR) to your PATH";; esac

uninstall:
	@for b in $(BINS); do rm -f "$(BINDIR)/$$b" && echo "removed: $(BINDIR)/$$b"; done

plugin-link: install
	herdr plugin link "$(CURDIR)/plugin"

plugin-unlink:
	-herdr plugin unlink herdr-wakeup

clean:
	cargo clean
