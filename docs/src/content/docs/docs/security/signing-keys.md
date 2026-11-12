---
title: Signing keys
description: Which keys sign Lodestore releases, where the project key is built in, how it is rotated and what to do if it leaks.
---
A release is signed so that you can check it came from the project. This page says what is signed with which key, where the project key sits, how it is replaced on a plan, and what you do if it is stolen.

## What is signed with what

| What | Signed with | How to check |
| --- | --- | --- |
| The six binaries, `SHA256SUMS` and the agents the server hands to new hosts | The project's minisign key, id `8CA38864EEA52F78` | [Check the files](/docs/install/binary/#check-the-files) |
| Agent updates | The same key; the agent checks it before it runs the new binary | [Agent updates](/docs/guides/agent-updates/) |
| The Docker image and the Helm chart | cosign, keyless: the signature names the release workflow `release.yml` of this repository as the signer, there is no key to keep | [Check the image](/docs/install/docker/#check-the-image), [Check the chart](/docs/install/helm/#check-the-chart) |
| Build provenance of the binaries and the image, SBOM of the image | GitHub attestations, issued to the release workflow | [Check the build provenance](/docs/install/binary/#check-the-build-provenance) |

The minisign key is kept as the secrets `MINISIGN_KEY` and `MINISIGN_PASSWORD` of the GitHub environment `release-signing`. Only the signing job of the release workflow reads them.

## Where the project key is built in

The public key is committed as `crates/lodestore-agent/keys/project.pub`, and copies of it sit in:

- the agent binary, which trusts it for updates;
- `install.sh`, which the server serves, for the signature of the agent it installs;
- the Add host wizard of the web interface, which shows it next to the commands that check an agent by hand;
- the page [Binary](/docs/install/binary/#check-the-files).

Before you rely on a key, compare its id with the tag of your release. Do not take the key from a mirror or from a message: the file in the repository at the tag and the id on the Binary page must be the same.

## What a host trusts

An agent trusts the key built into its binary and the `*.pub` files in `/etc/lodestore/trusted-keys` on the host. The server has no message that adds, replaces or removes a key, so a key reaches a host only through `install.sh`. A test, `two_host_keys_both_verify_and_a_third_does_not`, checks that an agent with two trusted keys takes a signature made with either one and refuses a third key.

Version 0.2.0 has one built-in key and no spare. The agent of 0.2.0 trusts that key and the files in `trusted-keys`; it does not carry a second key of its own.

## Planned rotation

A rotation that nobody forces is done in two releases, so that no host is left behind:

1. Release N is signed with the current key and carries the next public key beside `keys/project.pub`; its agent trusts both. This is a change of the agent, so it ships in a release and is listed in its notes.
2. From release N+1 on, releases are signed with the new key. The secrets of the `release-signing` environment, the key in `install.sh`, the Add host wizard and the Binary page change in the same release.
3. A host that is still on a version before N cannot take an update signed with the new key. Update it to N first, or run `install.sh` on it again.

The notes of release N say that a rotation is under way and give the id of the next key.

## If the key is stolen

Whoever holds the key can sign a file that looks like a release. An attacker who also controls the server, or a download, can then make an agent run it. The server alone cannot: the agent decides what it runs.

If the project learns that the key is exposed:

1. A security advisory names the key and the releases that must be treated as untrusted.
2. A new key is made and published as a new release. The agents that trust only the old key refuse an update signed with the new one.
3. On every host, run `install.sh` of the new release. It installs an agent binary that carries the new key. To trust a key without waiting for a release, give the installer the file:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://lodestore.example.com/install.sh | sh -s -- --trust-key ./new-key.pub
```

`--trust-key` puts the file into `/etc/lodestore/trusted-keys` as `root:root` with mode 0644, the only way a key gets there. Take the file from the advisory and compare its key id with the tag of the release and with the Binary page before you install it, because a key you add is trusted for updates of the agent that runs as root. A key that you added by mistake is removed by deleting its file from `/etc/lodestore/trusted-keys`.

The old key stays built into agent binaries that were installed earlier, and a trusted key cannot be taken out of them by an update. Installing the new release replaces the binary and with it the built-in key.
