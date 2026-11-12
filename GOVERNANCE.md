# Governance

Lodestore is a small project with one maintainer. This page says how it is run today, so you know what to expect.

## Maintainers

[@neverlless](https://github.com/neverlless) is the maintainer. They decide what is merged, review pull requests and publish releases.

## How decisions are made

Ideas, bugs and design questions are discussed in [issues](https://github.com/lodestore/lodestore/issues) and [Discussions](https://github.com/lodestore/lodestore/discussions). The maintainer decides after the discussion and explains the reasoning when a proposal is declined.

A change to the repository format or to the cryptography is written up first as an architecture decision record (ADR) in [`docs/src/content/docs/docs/adr/`](docs/src/content/docs/docs/adr/), and the code follows the ADR. [CONTRIBUTING.md](CONTRIBUTING.md) lists the extra rules for such changes.

## Becoming a maintainer

There is no application. A contributor who has made steady, good changes and reviews over time can be invited by the current maintainer to become a maintainer. Maintainers have write access and follow this page and the [Code of Conduct](CODE_OF_CONDUCT.md).

## If the maintainer steps away

The code stays under Apache-2.0, so anyone can keep it going as a fork. Nothing in your backups depends on the project staying alive or on a running server: a repository can be restored with only the bucket and the password or recovery key, and the repository format is described in the ADRs and in the source.

## Security

Vulnerabilities are reported privately, as described in [SECURITY.md](SECURITY.md).

## Code of conduct

Everyone taking part in the project follows the [Code of Conduct](CODE_OF_CONDUCT.md).
