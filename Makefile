.PHONY: check clippy docs

CARGO = cargo
CARGO_FEATURES = --all-features

CLIPPY_ALLOW = \
	clippy::upper_case_acronyms \
	dead_code

CLIPPY_OPTS = $(foreach a,$(CLIPPY_ALLOW),-A $a)

check:
	$(CARGO) check $(CARGO_FEATURES)

clippy:
	$(CARGO) clippy $(CARGO_FEATURES) -- $(CLIPPY_OPTS)

docs:
	$(CARGO) doc $(CARGO_FEATURES)
