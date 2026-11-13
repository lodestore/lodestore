---
title: "Agent updates"
description: "Update the Lodestore agent across your hosts in waves. The agent installs only a binary signed with a key it trusts and checks its version and architecture."
---
The server can replace the agent binary on your hosts. Nothing updates by itself: you start an update in **Hosts** (or with the `agents:update` API scope), pick one host, several, or all outdated ones, and watch it in **Jobs**. The agent runs as root, so it accepts only a binary that carries a valid signature from a key it trusts.

## How an update runs

1. The server finds the signed agent builds in its agent directory (`agent_dir`, `/usr/share/lodestore/agents` by default). The highest signed version is the target. A build counts as signed only when `lodestore-agent-linux-<arch>.minisig` sits next to it.
2. You start the update. Hosts connected at that moment go first, so the `canary` hosts of wave 0 can act at once; the rest follow in blocks of `wave`. A host with a backup or restore running waits for it, and no backup starts on a host while its update is in progress.
3. The agent downloads the binary and the signature over its own authenticated connection to the server. It checks the SHA-256 the server announced, the minisign signature, that the signed version is the requested one, that the architecture is its own, that the version is newer than the running one, that a stable agent is not asked to take a pre-release (see [Release channels](#release-channels)), and that the new binary prints its version when started with `version`. Any failure ends the update with the reason, and the running binary is not touched.
4. The agent keeps the old binary as `lodestore-agent.prev`, switches, arms a rollback timer for 120 seconds and restarts through systemd.
5. The new agent connects and reports its version. The update is then `succeeded`. If it does not report in 2 minutes, the timer puts `lodestore-agent.prev` back and the host reports the rollback with the reason, for example "The new version did not connect within 2 min. Running 0.1.0 again."

No later wave starts before at least one canary host succeeded. A busy canary host is waited for. If no canary host is left and none succeeded, because they rolled back, were revoked or were updated by hand, the rollout pauses and says so; resuming starts the next wave without a canary. The same pause follows when hosts wait in a later wave and, for `health_timeout_secs` after the start or the last resume, no canary host is connected, in progress or done. Canary hosts that stay offline are updated when they return, once the rollout runs again. A rollout with no later wave never pauses for this: its offline hosts simply update when they return.

After that the server starts the next wave only when no host of the update is still in progress. Hosts that are offline or busy do not hold a later wave back; they are updated when they come back. A host that reached the target or a newer version in the meantime, for example through `install.sh`, leaves the queue without counting as a rollback.

### Pausing

A rollout pauses itself when the rollbacks since the last continue are **more than** `pause_after`. With the default `pause_after: 1`, one rollback is tolerated and the second pauses the rollout; hosts already in progress finish. Resume it after you have looked at the reason in **Jobs**; resuming forgets the rollbacks so far. You can also pause, resume or cancel a rollout yourself.

A rollout also pauses when the signed binary of its target for a host's architecture is gone from `agent_dir`, for example because a newer version replaced it. The rollout shows the reason, the hosts stay queued and nothing counts as a rollback. Put the binary back and resume, or cancel and start a new update to the newer version.

## Release channels

A version with a suffix such as `-rc.1` is a release candidate; one without a suffix is stable. An agent's channel is its own version, and nothing configures it.

- A stable agent refuses a pre-release before it runs the downloaded binary: `Version 0.3.0-rc.1 is a pre-release and agent 0.2.0 takes only stable releases.`
- A release candidate agent takes a newer release candidate and a newer stable release. Leaving the release candidate channel is one-way: a host that took the stable release no longer takes release candidates, and only a new `install.sh` run puts it back on one.
- A server whose agent directory holds a pre-release does not show stable hosts as outdated and does not start an update for them, and it cancels such hosts that were already queued. Agents of 0.1 check no channel, so this filter is what protects them.
- A new host installed from that server gets the pre-release agent, because `install.sh` and the download for it do not check the channel.

## Settings

In `server.yaml`:

```yaml
agent_updates:
  canary: 2
  wave: 10
  pause_after: 1
  health_timeout_secs: 150
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `canary` | 2 | Hosts in the first wave. |
| `wave` | 10 | Hosts in each later wave. |
| `pause_after` | 1 | The rollout pauses when rollbacks since the last continue are more than this number. |
| `health_timeout_secs` | 150 | How long the server waits for a switched agent to report the new version. The time counts from the moment the agent reports the switch, so the download does not use it up. During the download an update fails only when it stops making progress for this long; a slow link alone does not. It must be at least 120, the time the agent has before it rolls itself back. |

All four must be greater than zero. The agent's own rollback timer is fixed at 120 seconds, shorter than the server's wait on purpose: the agent normally rolls back first.

## Signing your own builds

The agents of an official release trust the project key built into the binary. If you build the agent yourself, sign it with your own key and put that key on the hosts.

Create a key pair once with [minisign](https://jedisct1.github.io/minisign/):

```sh
minisign -G -p own.pub -s own.key
```

Sign each build. The trusted comment is part of the contract, `lodestore-agent <version> <arch>`: it names the product, the version and the architecture (`x86_64` or `aarch64`), and the agent compares it with what it was asked to install.

```sh
VERSION=0.2.0
ARCH=x86_64
minisign -S -s own.key -t "lodestore-agent $VERSION $ARCH" -m lodestore-agent-linux-$ARCH
```

- Sign in the default mode, which produces an `ED` signature over the hash of the file. Do not pass `-l`: legacy signatures are refused.
- Copy `lodestore-agent-linux-$ARCH` and the resulting `lodestore-agent-linux-$ARCH.minisig` into the server's `agent_dir`.
- The version in the comment must equal the version the binary prints, and must be higher than the version on the hosts. A lower or equal version is refused.

Check a binary before you publish it:

```sh
minisign -V -p own.pub -m lodestore-agent-linux-x86_64
```

For a release of the project use the project key (`project.pub`) the same way.

### Trusting your key on a host

Install the public key when you install the agent, or run the installer again later:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://lodestore.example.com/install.sh | sh -s -- --join <token> --trust-key ./own.pub
```

`--trust-key` may be repeated. The installer also accepts an agent signed by this key when it installs, so a build signed with your own key installs on a host with `minisign`. The file name must end in `.pub`, and two files may not share a name, because the installer keeps each key under its file name. The installer puts it in `/etc/lodestore/trusted-keys/` owned by root with mode 0644; the directory is root's with mode 0755. The agent ignores a key file that is a symlink, larger than 4 KiB, or writable by anyone but its owner, or whose directory is. `install.sh --uninstall` removes the agent and `lodestore-agent.prev`; `--purge` also removes the keys with `/etc/lodestore`.

An update cannot add a key to a host, so a key that was never installed there is not trusted through the update path.

## First update of existing agents

Agents installed before this version cannot update themselves. Run `install.sh` on each of these hosts once; from then on updates run from **Hosts** without SSH. In **Hosts** such a host shows "This agent can't update itself. Run install.sh on the host." The same message appears for an agent that does not run under systemd, or whose binary directory is not writable.

On an already registered host the installer replaces the binary and keeps the identity:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://lodestore.example.com/install.sh | sh
```

If an update is in progress on the host, the installer ends it: it stops the rollback timer and removes `lodestore-agent.prev`, so the timer does not replace the binary you installed.

When `minisign` is installed on the host, `install.sh` verifies the agent's signature before it installs the binary. The trusted comment must name the product (`lodestore-agent`) and the architecture, and the key must be the project key built into the script or one given with `--trust-key`. The version is not compared on this path. Without `minisign` the installer checks only the SHA-256 served by the same server, which shows the download is intact, not who built it, and prints a `note:` saying so. On a server you do not trust, verify the binary with `minisign -V` first, as under [Signing your own builds](#signing-your-own-builds).

## When an update fails

- **Rejected.** The signature, checksum, version or architecture did not pass, or the new binary did not start. The old agent keeps running and the reason is in **Jobs** under Agent update and in the **Hosts** page.
- **Rolled back.** The new version did not connect in time, or the host restarted in the window. The rollback restores the previous binary and the host stays online. If `lodestore-agent.prev` has disappeared, nothing can be restored: the host keeps the new binary and reports "The previous binary is gone, so the update was not rolled back. Run install.sh on the host." A rolled-back update raises the `agent_update_failed` alert and the `lodestore_host_agent_update_failed` metric until the host runs the target version; see [Alerts and notifications](/docs/guides/alerts/) and [Metrics](/docs/reference/metrics/).
- **Host went quiet.** If a rollout waits for a host that stays offline, it keeps the host in its queue and updates it when it comes back.

### Residual risk

The old agent arms the rollback timer before it restarts into the new binary, so a new binary that crashes or never connects is rolled back by the timer.

One gap remains. A reboot of the host inside the two-minute window loses the transient timer. The agent that starts after the reboot arms it again for the rest of the window, or rolls back at once when the window is over. If the new binary also dies before its start-up code (`on_start`) runs, nothing re-arms the timer and nothing rolls back. Only the server notices: after `health_timeout_secs` it records the update as rolled back by its deadline and raises `agent_update_failed`, or, if the report of the switch never reached it, the host shows as offline and `agent_offline` fires. Run `install.sh` on the host to repair it.

Signature checks protect the update path only. A server that is taken over can still start a restore job, which runs as root, but the agent writes only under its restore folders, by default `/var/lib/lodestore/restore`. The server is root on a host only where the agent allows in-place restore, where the agent is from 0.1 or where the restore folders include system directories. Hooks from the allowlist stay in its hands everywhere. See the [threat model](/docs/security/threat-model/#agent-updates).
