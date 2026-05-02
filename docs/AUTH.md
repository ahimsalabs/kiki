# Authentication and Identity

Design for kiki's identity, authentication, and daemon-to-daemon trust
model. Separated into layers so each can evolve independently.

**Depends on:** git convergence (for content-addressed storage of
identity data), protection rules (`REVIEW.md`).

**Companion doc:** [`REVIEW.md`](./REVIEW.md) — approvals, OWNERS,
protect.toml, the `land` flow.

## Principles

1. **Identity is portable.** The same identity string appears in
   approval blobs, OWNERS files, commit metadata, and group
   definitions. It never encodes the proof mechanism.

2. **Authentication is local.** How a daemon verifies identity is a
   per-daemon configuration choice. Different daemons in the same
   topology can use different auth methods.

3. **Authorization is in-repo.** What an identity can do is defined
   by version-controlled policy files that sync like code.

4. **Start simple, grow without rearchitecting.** Tailscale today,
   SSH-agent next, OIDC later — same data model throughout.

## Layer 1: Identity (the envelope)

An identity is an **email address**. That's the stable handle that
appears everywhere in the system:

- Approval blobs: `approver: "alice@co.com"`
- OWNERS files: `owners = ["alice@co.com"]`
- Commit metadata: `author: "Alice <alice@co.com>"`
- Submissions: `author: "alice@co.com"`
- Groups: `members = ["alice@co.com", "bob@co.com"]`

Why email:
- Already in every git/jj commit
- Human-readable
- Federated (no central registry needed to bootstrap)
- Maps directly to SSO/SAML identity providers
- Universally understood

### Groups

Groups provide indirection so OWNERS files and protection rules don't
hardcode individual emails:

```toml
# .kiki/groups.toml
[groups.backend]
members = ["alice@co.com", "bob@co.com", "carol@co.com"]

[groups.security]
members = ["alice@co.com", "dave@co.com"]

[groups.admin]
members = ["alice@co.com"]
```

OWNERS files and protection rules reference groups:

```toml
owners = ["group:backend"]
```

The daemon resolves group membership at enforcement time by reading
`groups.toml` from the current tip of the protected ref.

### Identity registry

The mapping from identity to authentication credential lives in-repo:

```
# .kiki/authorized_keys
# Format: <identity> <key-type> <public-key> [comment]
alice@co.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... alice-laptop
alice@co.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... alice-desktop
bob@co.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... bob-workstation
```

This file:
- Is version-controlled (auditable, reviewable)
- Syncs with the repo (every daemon gets the latest)
- Changes go through the same review process as code
- Supports multiple keys per identity (laptop + desktop)

Adding a team member:
```bash
echo 'dave@co.com ssh-ed25519 AAAA...' >> .kiki/authorized_keys
kiki describe -m "add dave to the team"
# goes through normal approval flow to land on main
```

Revoking access:
```bash
# Remove line from authorized_keys, goes through review
# Once landed on protected ref, daemon reloads — dave's future
# connections are rejected
```

For OIDC/SAML deployments, `authorized_keys` is replaced (or
supplemented) by trust in the IdP — the daemon doesn't need
per-user key entries because the IdP asserts identity.

## Layer 2: Authentication (sealing the envelope)

When the kiki CLI connects to a daemon, the daemon needs to resolve
the connection to an identity envelope. Multiple mechanisms, same
result.

### SSH agent (small teams, indie developers)

The natural starting point. Every developer already has SSH keys
and a running agent.

**Flow:**
1. CLI connects to daemon over gRPC
2. Daemon sends a challenge (nonce)
3. CLI signs the challenge via the local SSH agent (`SSH_AUTH_SOCK`)
4. Daemon receives signature + public key
5. Daemon looks up the public key in `.kiki/authorized_keys`
6. If found → session stamped with the associated identity

**Advantages:**
- Zero new infrastructure
- Developers already have keys
- Works offline (no IdP dependency)
- ssh-agent handles key lifecycle (hardware keys, passphrases)

**CLI config:**
```toml
# ~/.config/kiki/config.toml (or jj config)
[auth]
method = "ssh-agent"
identity = "alice@co.com"   # which identity to present
```

### OIDC/SAML (organizations)

For teams that use Google Workspace, Okta, Azure AD, etc.

**Flow:**
1. CLI initiates OAuth device flow (or browser redirect)
2. User authenticates with IdP
3. CLI receives an ID token asserting `alice@co.com`
4. Token stored in system keychain (macOS Keychain, Linux
   secret-service, Windows Credential Manager)
5. On connect, CLI presents token to daemon
6. Daemon validates token signature against IdP's JWKS
7. Session stamped with the email claim from the token

**Advantages:**
- Maps to existing org identity (no separate key management)
- Token refresh is automatic
- Revocation is instant (disable in IdP → token invalid)
- No per-user public keys to manage

**Daemon config:**
```toml
# daemon.toml
[auth.oidc]
issuer = "https://accounts.google.com"
client_id = "1234.apps.googleusercontent.com"
allowed_domains = ["co.com"]
```

### mTLS client certificates (infrastructure-heavy orgs)

For deployments with existing PKI (internal CAs, cert-manager, etc.)

**Flow:**
1. Internal CA issues a client cert with identity in the SAN
   (e.g., `URI:kiki://alice@co.com`)
2. CLI presents cert on TLS handshake
3. Daemon validates cert chain against trusted CA
4. Session stamped with identity from SAN

**Advantages:**
- Zero user interaction after cert provisioning
- Mutual authentication (client verifies server too)
- Works well with service mesh / infrastructure automation

### Tailscale identity (pragmatic zero-config)

If all traffic flows over Tailscale, identity comes for free.

**Flow:**
1. Daemon listens on Tailscale interface
2. On connection, daemon queries Tailscale local API for peer identity
3. Tailscale reports the peer's node identity and associated user
4. Session stamped with the Tailscale user's email

**Advantages:**
- Absolutely zero configuration
- No keys, tokens, or certs to manage
- Encrypted + authenticated by Tailscale
- Device-level identity (hardware-bound)

**Daemon config:**
```toml
# daemon.toml
[auth]
methods = ["tailscale"]   # just works if on a tailnet
```

### Method precedence

The daemon config declares which methods are accepted and in what
order:

```toml
# daemon.toml
[auth]
methods = ["tailscale", "ssh-agent", "oidc"]
# First match wins. Methods tried in order.
```

A daemon that only talks over Tailscale needs no further config.
A daemon exposed to the internet would use `["oidc"]` or
`["mtls"]`.

## Layer 3: Authorization

Covered in [`REVIEW.md`](./REVIEW.md). Summary:

- `protect.toml` — per-ref rules (immutable, append_only,
  required_reviewers)
- `OWNERS` — per-path ownership
- `groups.toml` — team membership

All in-repo, version-controlled, evaluated at enforcement time by
the daemon receiving the `cas_ref`.

## Daemon-to-daemon trust (secure sync)

Separate from user identity. Concerns: how do daemons authenticate
each other for replication?

### Tailscale (now)

Daemons on the same tailnet trust each other implicitly. Tailscale
provides encrypted, authenticated channels. No additional config.

This is the recommended starting point. It sidesteps TLS cert
management entirely.

### mTLS between daemons (future)

For deployments without Tailscale:

- Each daemon has a server cert + key
- A shared CA (or the team daemon's cert as trust root)
- Peer daemons present client certs on connect
- Standard TLS mutual authentication

### Trust-on-first-use (TOFU)

For initial setup without pre-shared trust:

```bash
kiki kk init "grpc://team-server:12000" my-project
# First connect: "Server fingerprint is sha256:ABCDEF...
#                 Trust this server? [y/N]"
```

Accepted fingerprints are stored in-repo:

```toml
# .kiki/remotes.toml
[remotes.team]
url = "grpc://team.tailnet:12000"
fingerprint = "sha256:ABCDEF1234567890..."
```

This file syncs, so once one team member trusts the server,
everyone gets the fingerprint. Key rotation goes through normal
review.

## Approval signing

When a user issues an approval (`kiki kk approve <change>`), the
approval blob must be signed. Who signs depends on topology:

### Team daemon signs (hub-and-spoke)

The team daemon is the attestation authority:

1. CLI sends `Approve` RPC to team daemon
2. Daemon verifies caller identity (via Layer 2)
3. Daemon constructs approval blob
4. Daemon signs with its own key (the server key)
5. Stores blob in CAS, sets ref

Any peer can verify the approval by checking the team daemon's
public key (stored in `.kiki/remotes.toml` or the repo's trust
config).

The approval blob:
```
Approval {
  change_id: ChangeId,
  commit_id: CommitId,
  approver: "bob@co.com",        // the identity
  attestor: "team.co.com",       // who signed (the daemon)
  timestamp: DateTime,
  signature: Bytes,              // team daemon's signature
}
```

### Self-signed (peer-to-peer)

Without a central daemon, the user signs directly:

1. CLI constructs approval blob
2. CLI signs via SSH agent (same key used for auth)
3. Stores blob in CAS, sets ref

```
Approval {
  change_id: ChangeId,
  commit_id: CommitId,
  approver: "bob@co.com",
  attestor: "bob@co.com",        // self-signed
  timestamp: DateTime,
  signature: Bytes,              // bob's SSH key signature
}
```

Verification: the daemon looks up Bob's public key in
`.kiki/authorized_keys` and verifies the signature.

### Verification at enforcement time

When a `cas_ref` is checked against protection rules:

1. Load approval blobs from `refs/approvals/<change_id>/*`
2. For each approval:
   - If `attestor` is a known daemon → verify against daemon's
     public key from `.kiki/remotes.toml`
   - If `attestor == approver` → verify against the approver's
     public key from `.kiki/authorized_keys`
3. Reject approvals with invalid signatures
4. Check remaining valid approvals against OWNERS requirements

## Staged buildout

### Phase 1: No identity (now)

- Transport security via Tailscale
- Protection limited to `immutable` and `append_only` in daemon.toml
- No per-user enforcement — rules apply to everyone equally
- No approvals

### Phase 2: SSH-agent identity

- `.kiki/authorized_keys` in-repo
- SSH challenge-response on connect
- Daemon stamps sessions with identity
- Approvals become possible (team-daemon-signed)
- OWNERS enforcement works
- `kiki kk approve`, `kiki kk land` available

### Phase 3: OIDC/SAML

- Alternative auth method for orgs
- Token-in-keychain flow
- Same identity envelope, different seal
- No changes to data model or authorization layer

### Phase 4: Daemon-to-daemon mTLS

- For non-Tailscale deployments
- Client cert auth as alternative to SSH-agent
- Daemon cert rotation via in-repo config

## Open questions

1. **Key rotation.** When a developer gets a new laptop, they add a
   new key to `authorized_keys`. The old key should be removed. Is
   there a grace period, or is removal instant upon landing?

2. **Emergency access.** If all admins are locked out (keys lost),
   how do you recover? Probably: the daemon has a local-only
   emergency recovery mechanism (similar to `break glass` in cloud
   IAM).

3. **Scope of identity.** Is identity per-repo or global? If Alice
   has access to repos A and B on the same daemon, is it one
   `authorized_keys` per repo or one per daemon? Current design
   says per-repo (in-repo config), which means adding Alice to a
   new repo requires a commit to that repo's `.kiki/` directory.

4. **Offline approvals in team mode.** If the team daemon signs
   approvals, can Bob approve while disconnected? Probably not in
   team mode — the signature requires the daemon. In peer mode
   (self-signed), yes.

5. **Audit log.** Should the daemon maintain a tamper-evident log of
   all authentication events and ref advances? Useful for
   compliance but adds complexity.
