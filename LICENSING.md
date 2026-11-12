# Licensing

## What is under which license

All code and documentation in this repository is licensed under Apache-2.0 (SPDX `Apache-2.0`); the text is in [LICENSE](LICENSE). That covers every crate in `crates/` (`lode-cli`, `lodestore-agent`, `lodestore-format`, `lodestore-fuzzkit`, `lodestore-policy`, `lodestore-proto`, `lodestore-server`, `lodestore-testkit`), the web interface in `web/`, and the site and documentation in `docs/`.

## Third-party files

Some files come from other projects and keep their own licenses, each next to the files it covers:

- `crates/lodestore-server/assets/scalar/`: the Scalar API reference that the server serves, under the MIT license (`LICENSE` in that folder).
- `docs/src/assets/fonts/`: the Onest and JetBrains Mono fonts of the site, under the SIL Open Font License (`OFL.txt` in that folder).
- `web/src/assets/fonts/`: the same fonts for the web interface, under the SIL Open Font License (`OFL.txt` in that folder).

## Contributions

Contributions are made under the [Developer Certificate of Origin](https://developercertificate.org/): every commit carries a `Signed-off-by` line, added by `git commit -s`, and the `dco` check in CI refuses a pull request with a commit that lacks one. A contribution is accepted under Apache-2.0. See [CONTRIBUTING.md](CONTRIBUTING.md).

## History

In 0.1 the server (`lodestore-server`) was released under AGPL-3.0-only and the other crates under Apache-2.0. From 0.2 the server is Apache-2.0 too, so that the whole project is under one license. The 0.1 releases stay under the licenses they were released with.
