SHELL := /bin/bash
.SHELLFLAGS := -o pipefail -c

E2E_MEMORY ?= 16G
E2E_LOG ?= /tmp/davinci-e2e.log
# Node datadirs (kept on failure) go here rather than /tmp, which is often
# a small tmpfs.
TMPDIR ?= $(HOME)/.cache/davinci-e2e

.PHONY: build test e2e

build:
	cargo build --workspace

test:
	cargo test --workspace

# Full acceptance test: anvil, the contracts, three nodes plus an observer,
# the prover at DAVINCI_ZKVM_URL. Runs in a memory-capped scope: the test's
# child processes (anvil, nodes) are only killed on Drop, so a crash of the
# test binary must not leave them running unbounded.
e2e:
	mkdir -p $(TMPDIR)
	systemd-run --user --scope -p MemoryMax=$(E2E_MEMORY) -- \
		env DAVINCI_E2E=1 TMPDIR=$(TMPDIR) cargo test -p davinci-e2e --test e2e -- --nocapture \
		2>&1 | tee $(E2E_LOG)
