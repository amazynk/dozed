# Contributing

Bugs go to [issues](https://github.com/amazynk/dozed/issues/new/choose), and security
problems go privately, as [SECURITY.md](SECURITY.md) describes. Everyone taking part is
expected to follow the [code of conduct](CODE_OF_CONDUCT.md).

## Before you send a change

`DESIGN.md` is the long form: read the section your change touches first, because most of
the non-obvious choices are there on purpose. Then these three have to pass, as they do in
CI:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`packaging/test-install.sh` checks the installer against a throwaway tree, and
`packaging/verify-matrix.sh` runs the daemon against headless Sway and nested X11 where
those are installed.

## Commits

`git config commit.template .gitmessage` gives you the shape below each time.

**One commit does one thing.** The test for whether it is one thing: can
the subject say what changed without the word "and" joining two
unrelated halves? If not, it is two commits. Work that genuinely cannot
be separated — a change that does not compile without another — is one
commit whose body says so in as many words, rather than a bundle nobody
mentions.

**Subject: `area: what changed`, 72 characters at the outside.** Written
so that `git log --oneline | grep` finds it later. The subject is an
index entry, not an epigram; if a sentence is worth writing for its own
sake, it is the first line of the body, where it costs nobody a search.

**Body: what this changed, why, and what it cost.** Evidence with
numbers where there is any — what was measured, on what, and what the
alternative would have been. Negative results are worth committing:
"this was tried, here is the measurement, here is why it is not in the
tree" saves the next person the same afternoon.

**Standing facts belong in `DESIGN.md`, not in a commit body.** A commit
describes the day it was made; it is never edited again, and nothing
greps it when a default changes. Anything that has to stay true — what a
setting means, what the default is, what a section guarantees — goes in
`DESIGN.md`, and the commit points at the section. A commit body that
asserts a current fact is a fact that will rot silently.

**Every commit ends with a `Tested:` trailer.** The exact command and
the exact result:

    Tested: cargo test --workspace -> 863 passed, 13 ignored, 0 failed

If it was not run, the trailer says so and why — `Tested: not run;
$reason` — so that "tests pass" is never a phrase somebody has to take
on trust. `packaging/test-install.sh` and `packaging/verify-matrix.sh`
belong in the trailer too when the change reaches them.

**Keep it reviewable.** A commit over a few hundred lines of diff is one
nobody reads line by line. Prefer the seam that makes it two.

## Releases

* `CHANGELOG.md` follows [Keep a Changelog]; the entry lands in the same
  commit as the change where that is practical, and in the release
  commit otherwise.
* Tag every release `vX.Y.Z`, on `main`.
* Below 1.0 the public surface is the config file, the IPC protocol and
  the CLI. Those three are what a minor bump may move; anything that
  moves one of them without a bump is a bug.
* `DESIGN.md` §16 records what was verified against a live session or a
  real kernel, and what was not. A release does not change that section
  by wishing.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
