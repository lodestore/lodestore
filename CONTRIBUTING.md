# Contributing to Lodestore

Thank you for taking the time. Bug reports, fixes, documentation and tests are all welcome. For anything larger than a fix, open an issue first so that we can agree on the approach before you write code.

To report a vulnerability, follow [SECURITY.md](SECURITY.md) and do not open a public issue.

## Building and testing

You need the Rust toolchain pinned in `rust-toolchain.toml` (the workspace builds with Rust 1.94 or newer, edition 2024) and [cargo-nextest](https://nexte.st/). The web interface needs Node.js 24 and pnpm; the documentation site needs the same.

```sh
cargo build --workspace
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo tree -i openssl-sys
```

The last command must report that `openssl-sys` did not match any packages: Lodestore uses rustls with aws-lc-rs and no OpenSSL. Do not add `native-tls` or `ring`.

Tests that talk to S3 download a pinned MinIO release and check its sha256; they need network access the first time. Tests that need root, extended attributes or systemd are marked `#[ignore]` and run with `scripts/test-linux.sh --root` on a Linux machine. Tests against PostgreSQL run with `scripts/test-postgres.sh`.

The web interface and the documentation:

```sh
pnpm --dir web install --frozen-lockfile
pnpm --dir web test
pnpm --dir web lint
pnpm --dir docs install --frozen-lockfile
pnpm --dir docs build
pnpm --dir docs check
```

The documentation build also checks every internal link. `pnpm --dir docs check` runs on the built site and checks links and anchors, that the pages load nothing from outside, that every light image has its dark pair, the weight budget, the facts on the home page, robots.txt, the icons and the structured data of the home page, and that every page carries a content security policy meta tag with hashes of its own inline scripts. Inline event handlers (`onclick=` and the like) are not allowed on the site, and the build fails on them.

## Fuzzing, mutation testing and the browser test

The fuzz targets are in `fuzz/fuzz_targets/` and the code they run is in `crates/lodestore-fuzzkit`. The inputs saved from past crashes are replayed on stable by the normal tests:

```sh
cargo nextest run -p lodestore-fuzzkit
```

To fuzz, you need Linux, the nightly pinned in `fuzz/rust-toolchain.toml` and `cargo install --locked cargo-fuzz --version 0.13.2`:

```sh
cd fuzz
cargo fuzz run <target> -- -max_total_time=60 -rss_limit_mb=2048 -malloc_limit_mb=256 -max_len=1048576
```

A crash writes its input to `fuzz/artifacts/<target>/`. Reproduce it with `cargo fuzz run <target> <file>`, shrink it with `cargo fuzz tmin <target> <file>`, fix the code, and commit the shrunk input to `fuzz/regressions/<target>/` in the same pull request. A new target is a function in `crates/lodestore-fuzzkit/src/lib.rs` listed in `TARGETS`, a file in `fuzz/fuzz_targets/`, a `[[bin]]` in `fuzz/Cargo.toml` and a seed in `seeds.rs`; the tests in `crates/lodestore-fuzzkit/tests/regressions.rs` fail when a target has no file in `fuzz/fuzz_targets/` or no seed.

Mutation testing is slow, so run it on one file:

```sh
cargo mutants --file crates/lodestore-format/src/index.rs
```

The result of the weekly run is compared with `.github/mutants-baseline.json` by `scripts/mutants-ratchet.sh`. A mutant that survives is a missing or weak test: add the test. `#[mutants::skip]` is for a mutant that cannot change any observable behavior, and it needs a `//` comment with the reason on the line above. The baseline only goes down: `scripts/mutants-ratchet.sh --lower-only .github/mutants-baseline.json origin/main` shows what a pull request would loosen, and CI runs it.

The browser test drives the web interface through the main path. It needs Linux with Docker and systemd, because it installs an agent on the machine:

```sh
scripts/quickstart-ci.sh <image> --e2e
```

The script starts MinIO and the server image, runs `pnpm e2e` in `web/` (Playwright with Chromium, installed with `pnpm --dir web exec playwright install chromium`) and then compares the restored files with the originals. `pnpm --dir web e2e` runs the test alone against a stand the script has set up; it reads the addresses and keys of that stand from `E2E_*` variables and stops with a message when one is missing.

## Screenshots, clips and the terminal recording

The screenshots, the short clips and the terminal recording in `docs/src/assets/media/` are rendered by hand before a release and committed. CI does not render them.

You need Docker or OrbStack, `brew install vhs webp`, and an OrbStack machine named `lodestore-test` with systemd (the one `scripts/test-linux.sh` uses): the recording installs the agent in it, and the run stops the machine when it ends.

```sh
pnpm --dir docs/demo install --frozen-lockfile
pnpm --dir docs/demo exec playwright install chromium
pnpm --dir docs/demo render
```

The command starts a demo stand with generated passwords, captures every frame and records the terminal session, then removes the stand. Commit the files it writes to `docs/src/assets/media/`.

The site icons in `docs/public/` are redrawn from the mark with `pnpm --dir docs/demo icons` after the mark changes; commit the files it writes.

## Sending a change

1. Fork the repository and create a branch from `main`. Name it `<type>/<short-english-slug>`, where the type is one of `feat`, `fix`, `docs`, `refactor`, `test`, `perf` or `chore`, for example `fix/restore-symlink-target`.
2. Keep the change focused. One logical change per pull request, and several small commits are better than one large one. Commit messages are short, in English, and say what changed and why.
3. Add or update tests for the behavior you changed. If you change behavior that the documentation describes, change the documentation in the same pull request.
4. Run the commands above for the parts you touched. CI runs the same checks on every pull request.
5. Open a pull request against `main`. Describe the problem, what you changed and how you checked it.

Every commit is signed off under the [Developer Certificate of Origin](https://developercertificate.org/): a `Signed-off-by: Your Name <you@example.com>` line, added by `git commit -s`. The name and email must match the author of the commit. CI checks every commit of a pull request. For commits you already made, run `git rebase --signoff main`.

By submitting a change you agree that it is licensed under the [License](#license) below.

## Changes to the repository format

The repository format is frozen by committed data, because a silent change to chunk boundaries or encoding breaks deduplication and old repositories.

- Do not change the chunker, the encoding, the key derivation or the ciphers casually. `fastcdc` is pinned to an exact version for this reason.
- A change that alters any output must update the golden vectors in `crates/lodestore-format/tests/vectors.rs` deliberately, and explain in the pull request why the change is needed.
- Files under `crates/lodestore-format/tests/fixtures/v1` may only be added to, never modified, deleted or renamed; CI enforces this with `scripts/check-fixtures-append-only.sh`. A format change that old repositories cannot survive ships as `format_version` 2 with its own fixtures next to the old ones.

## License

The whole repository is licensed under Apache-2.0 (`LICENSE`), and a contribution is accepted under the same license.

## Security-sensitive code

Code that touches the repository format, cryptography, keys, deletion and cleanup, restore, agent privileges or authentication gets a closer review, and may take longer to merge. The agent must never gain the ability to delete data: deletion and cleanup are server-only, and must stay compatible with S3 Object Lock. The [threat model](https://lodestore.dev/docs/security/threat-model/) describes the boundaries.
