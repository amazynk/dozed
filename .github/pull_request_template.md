## What and why

<!-- What this changes, and the problem it solves. Link the issue if there is one. -->

## How it was tested

<!-- The exact commands and their results, as in the commit's `Tested:` trailer. -->

- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] `packaging/test-install.sh` and `packaging/verify-matrix.sh` pass, if the change reaches the installer or a focus source
- [ ] `DESIGN.md` says what is now true, if a default, a rule or a guarantee changed
