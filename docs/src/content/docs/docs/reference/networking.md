---
title: "Networking and reverse proxies"
---
The server listens on two ports with different rules:

| Port | Protocol | Default | Put behind a proxy as |
| --- | --- | --- | --- |
| REST and UI | HTTP | `listen` | an ordinary HTTPS reverse proxy that terminates TLS |
| gRPC for agents | HTTP/2 with mTLS | `grpc.listen` | a layer 4 (TCP) or TLS passthrough forward, never a TLS-terminating proxy |

## REST and UI: terminate TLS on a proxy

The server has no built-in TLS on the REST port. Put it behind a proxy that terminates TLS and set the public address so links and cookies are right:

```yaml
public_url: https://lodestore.example.com
trusted_proxies: [10.0.0.0/8]
```

- `public_url` is a bare origin: `http://` or `https://`, a host and an optional port, no path. A trailing slash is removed.
- When a request arrives over plain HTTP, the UI shows a warning banner.
- The session cookie gets the `Secure` flag when `public_url` starts with `https://`.
- Any ordinary reverse proxy works: nginx `proxy_pass`, Caddy, Traefik, a Kubernetes Ingress. Browser requests that change data are accepted when their `Origin` is the `public_url` origin or the host the request was addressed to, so the proxy does not have to pass the original `Host`.
- Sign-in attempts are limited per client address and username, and only failed attempts count. IPv6 clients are grouped by their /64 prefix. By default the client address is the address of the TCP connection, so behind a proxy all clients share the proxy's address and its limit.
- Set `trusted_proxies` (also `LODESTORE_TRUSTED_PROXIES`, comma separated) to the addresses or CIDR ranges of your reverse proxies. For connections from those addresses only, the server takes the client address from `X-Forwarded-For` (the closest entry that is not itself a trusted proxy). The proxy must set or append that header itself; never list a network that untrusted clients can connect from.

## gRPC: layer 4 or TLS passthrough

Agents authenticate with client certificates, and the server also pins the certificate authority in the join token. A proxy that terminates TLS replaces the client certificate and breaks both. An HTTP/2 ingress that terminates TLS therefore does not work.

Use one of:

- nginx `stream` with `proxy_pass` and no `ssl` on the listener
- Traefik `IngressRouteTCP` with `tls.passthrough: true` and a `HostSNI` rule
- a Kubernetes `Service` of type `LoadBalancer` or `NodePort` pointing at the gRPC port (the Helm chart creates `<release>-grpc` for this)

Agents keep one long-lived stream open and send HTTP/2 keepalive pings every 20 seconds. If you put a TCP proxy in between, set its idle timeout above 60 seconds so the stream is not cut.

### Addresses and certificate names

- `grpc.public_address` is the `host:port` agents connect to. It goes into the server certificate as a subject alternative name and into every join token.
- `grpc.extra_sans` adds more names or addresses to the certificate, for example a load balancer name or an IP that differs from `public_address`.
- If `grpc.public_address` is not set, the server uses the machine name and logs a warning; set it explicitly.

### Built-in CA or an external certificate

By default the built-in CA issues the server certificate, and the join token carries the CA fingerprint so the agent pins it. No other setup is needed.

To use a certificate from your own CA or from ACME and cert-manager, set both:

```yaml
grpc:
  tls_cert_file: /etc/lodestore/tls/tls.crt
  tls_key_file: /etc/lodestore/tls/tls.key
```

The install command is then generated without a CA fingerprint, and the agent verifies the server name against the system trust store. For a private CA, give the agent its bundle with `--ca-bundle <file>` on `lodestore-agent enroll`. Client certificates for agents are still issued by the built-in CA in both modes.

### Certificate rotation

The server checks the certificate files once a minute. When both files change and load as a matching pair, it starts serving the new certificate without a restart: connected agents are asked to reconnect and do so within a few seconds. A pair that does not load, for example while one of the two files is still being replaced, is ignored and the running certificate stays in use.

The built-in server certificate is valid for a year and is renewed automatically 30 days before it ends. Agents pin the CA, not the server certificate, so they are not affected.

## Agent certificates and offline hosts

Each agent certificate lives 90 days and is renewed after two thirds of that time, about day 60. A host that is switched off for longer than the remaining 30 days misses the renewal window and its certificate expires; it must be registered again with `install.sh --force` and a new join token. A host whose clock is wrong is not affected permanently: the agent keeps retrying with backoff and recovers once the clock is right.

## Installing agents

The install command shown in the UI downloads the installer and, through it, the agent binary and its checksum from the server. Over plain HTTP anyone on the network can get root on the host: a person on the path can replace the script, the binary and the checksum together, so the installer refuses `http://` downloads unless you pass `--insecure`. Use an `https://` `public_url` for anything beyond a trusted lab network. When `public_url` is `http://`, the generated command already contains `--insecure`, and the UI warns about the unsafe connection.

The installer downloads with `curl --proto '=https' --tlsv1.2` and does not follow redirects to plain HTTP.

When `minisign` is installed on the host, the installer also checks the agent's signature against the project key built into the script, and against any key you pass with `--trust-key`. Over plain HTTP that key arrives in the same script, so an attacker who can replace the script can replace the key too; the signature check protects you only when the script itself arrives over HTTPS. Without `minisign` the installer checks only the checksum from the server and prints a `note:` saying so.

`--from-github` fetches the agent, `SHA256SUMS` and `SHA256SUMS.minisig` from the GitHub release instead, which is HTTPS as well. It requires `minisign` and stops before any download when it is missing. To check an agent by hand before installing, see [Check the agent by hand](/docs/quickstart/#check-the-agent-by-hand).

`install.sh --force` keeps the previous registration until the new one succeeds; if enrollment fails or the installer is interrupted, the old identity is restored and the agent is restarted.

## Agents behind an HTTP proxy

The agent honours `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` (and the lowercase forms) for both gRPC and S3. It opens an HTTP `CONNECT` tunnel to the target and runs mTLS inside the tunnel.

Set them when installing; the installer saves them to `/etc/lodestore/agent.env` (mode 0600) and the systemd unit reads that file:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://lodestore.example.com/install.sh | HTTPS_PROXY=http://proxy.example.com:3128 sh -s -- --join <token>
```

To change the proxy later, edit `/etc/lodestore/agent.env` and run `systemctl restart lodestore-agent`. Add the server's address to `NO_PROXY` if it is reachable directly.

Proxies that perform TLS inspection are not supported: they cannot see inside an mTLS tunnel without replacing the certificates the agent pins and presents. Exempt the Lodestore server and the S3 endpoint from inspection.

## Other requirements

- **SQLite** needs a local disk. Do not put the data directory on NFS or SMB: file locking there is unreliable and corrupts the database under load. Use PostgreSQL for shared storage.
