# Security policy

## Supported versions

Security fixes are made for the latest release of the current minor version, which is 0.1.x. Older versions do not receive fixes; upgrade to the latest release.

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/lodestore/lodestore/security/advisories/new). Do not open a public issue or pull request for a security problem.

Include what you found, the version, how to reproduce it, and what an attacker gains. A minimal reproduction helps most. We will acknowledge the report, keep you informed while we work on it, and agree a disclosure date with you once a fix is ready. We credit reporters in the advisory unless you prefer otherwise.

## What is in scope

- The repository format and its cryptography: key handling, encryption, integrity of packs, indexes and snapshots, and anything that makes data readable or modifiable without the keys.
- The server: authentication, sessions, API tokens and scopes, the secrets it stores, the REST API and the web interface.
- The agent and its enrollment: the join token, certificates, update signature checks, backup hooks, and what a restore job can write on a host.
- Deletion and cleanup: anything that lets an agent, a token or a stranger delete or overwrite data that the design says only the server may remove.
- The install script, the release binaries and their signatures, the container image and the Helm chart.
- `lode`, the command line client, including a restore that writes outside its target.

## What is out of scope

- Findings that need root on a host that already runs an enrolled agent, or control of the server host or its unlock key together with its database. The [threat model](https://lodestore.dev/docs/security/threat-model/) states what an attacker gets in each of these cases.
- The quickstart serving plain HTTP, which is documented; put the server behind HTTPS.
- An agent key that can overwrite objects on MinIO, which the documentation already describes; use bucket versioning or Object Lock there.
- Denial of service by a user who already holds administrator rights.
- Vulnerabilities in a dependency that do not affect Lodestore; report those upstream.
- `demo_mode` on a server that holds real backups. It is for throwaway servers with synthetic data only.

## Further reading

- [Threat model](https://lodestore.dev/docs/security/threat-model/): what the server stores, what each kind of attacker gets, and the boundaries of agents, updates, hooks and tokens.
- [How we test data safety](https://lodestore.dev/docs/security/how-we-test/): which automated checks exist and what they do not cover.
