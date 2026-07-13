# herdr-wakeup - build the Herdr watcher binary.
#
#   make               # build release binary (target/release/wakeup-herdr)
#   make plugin-link   # build + link this plugin into Herdr for local development
#
# wakeup-herdr is an internal implementation detail of this plugin, not a
# user-facing CLI: it is never installed to a PATH directory. The plugin's
# own action scripts (plugin/bin/*) find it at target/release/wakeup-herdr,
# relative to this repo, via resolve_bins() in plugin/bin/lib.sh - the same
# way `herdr plugin install` builds it in place via bin/build.
#
# The standalone `wakeup` binary is vendored for supported macOS/Linux
# platforms. No separate install is needed there.

.PHONY: all build plugin-link plugin-unlink clean

all: build

build:
	cargo build --release --locked

plugin-link: build
	herdr plugin link "$(CURDIR)/plugin"

plugin-unlink:
	-herdr plugin unlink herdr-wakeup

clean:
	cargo clean
