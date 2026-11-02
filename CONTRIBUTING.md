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
```

The documentation build also checks every internal link.

## Sending a change

1. Fork the repository and create a branch from `main`. Name it `<type>/<short-english-slug>`, where the type is one of `feat`, `fix`, `docs`, `refactor`, `test`, `perf` or `chore`, for example `fix/restore-symlink-target`.
2. Keep the change focused. One logical change per pull request, and several small commits are better than one large one. Commit messages are short, in English, and say what changed and why.
3. Add or update tests for the behavior you changed. If you change behavior that the documentation describes, change the documentation in the same pull request.
4. Run the commands above for the parts you touched. CI runs the same checks on every pull request.
5. Open a pull request against `main`. Describe the problem, what you changed and how you checked it.

By submitting a change you agree that it is licensed under the license of the crates it touches (see below).

## Changes to the repository format

The repository format is frozen by committed data, because a silent change to chunk boundaries or encoding breaks deduplication and old repositories.

- Do not change the chunker, the encoding, the key derivation or the ciphers casually. `fastcdc` is pinned to an exact version for this reason.
- A change that alters any output must update the golden vectors in `crates/lodestore-format/tests/vectors.rs` deliberately, and explain in the pull request why the change is needed.
- Files under `crates/lodestore-format/tests/fixtures/v1` may only be added to, never modified, deleted or renamed; CI enforces this with `scripts/check-fixtures-append-only.sh`. A format change that old repositories cannot survive ships as `format_version` 2 with its own fixtures next to the old ones.

## Licenses and crate boundaries

| Component | License |
|---|---|
| `lodestore-agent`, `lodestore-format`, `lodestore-proto`, `lode-cli`, `lodestore-policy`, `lodestore-testkit` | Apache-2.0 |
| `lodestore-server` | AGPL-3.0-only |

Apache-2.0 crates must never depend on `lodestore-server` or any other AGPL crate, not even as a dev-dependency. `scripts/check-license-boundary.sh` fails the build if one does.

## Security-sensitive code

Code that touches the repository format, cryptography, keys, deletion and cleanup, restore, agent privileges or authentication gets a closer review, and may take longer to merge. The agent must never gain the ability to delete data: deletion and cleanup are server-only, and must stay compatible with S3 Object Lock. The [threat model](https://lodestore.github.io/lodestore/security/threat-model/) describes the boundaries.
