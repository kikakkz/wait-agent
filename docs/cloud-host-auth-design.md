# Cloud-Host Control-Plane Authentication Design

**Status:** Accepted  
**Approach:** Strengthened TOFU pinning + secure operator-key storage  
**Authorizes:** `task.cloud-auth-cert-pinning-design` and any implementation slices derived from it.

## 1. Problem

When a control host manages a cloud/public remote peer, both ends must authenticate each other over the gRPC control plane:

- The control host must know it is talking to the intended remote daemon.
- The remote daemon must know it is accepting commands from an authorized control host.
- Credentials must be manageable: generated, stored, rotated, and revoked without re-installing the remote OS.

The current implementation already works end-to-end, but the security model is implicit. This document makes it explicit and adds the missing lifecycle and storage hardening.

## 2. Current Baseline

The existing code (v0.1.32) already has the following pieces:

- **Remote daemon certificate:** generated on the remote host by `waitagent ... __generate-node-credentials`. It is self-signed.
- **TLS pin:** the control host stores the SHA-256 hash of the certificate's SPKI in `RemoteHostProfile.tls_pin_sha256` and verifies it with `TlsPinConnector` / `PinnedCertVerifier` (`src/infra/remote_grpc_transport.rs`).
- **Operator challenge:** the remote daemon sends a random challenge during the gRPC handshake; the control host signs it with its operator private key; the remote daemon verifies the signature against the operator public key installed under `~/.waitagent/authorized_operators/<fingerprint>.pub`.
- **Profile storage:** `~/.waitagent/remote-hosts.toml` stores host profiles, including the TLS pin.

This is effectively a trust-on-first-use (TOFU) model: the first SSH bootstrap is trusted because it runs inside the user's authenticated SSH session; every later gRPC connection verifies the same certificate.

## 3. Goals

- Make the TOFU pinning model explicit and auditable.
- Unify all local secrets (operator private key, SSH password, sudo password) in the OS keyring.
- Protect credentials from casual extraction; do not fall back to unencrypted file storage.
- Make operator-key rotation possible without re-imaging remote hosts.
- Add a remote daemon certificate rotation policy so certs do not live forever.
- Handle certificate expiry that occurs while the control host is stopped.
- Keep LAN host behavior unchanged (password or key auth allowed).
- Avoid requiring the control host to be online to issue or renew credentials.

## 4. Non-Goals

- Replacing SSH authentication for the bootstrap phase.
- A full PKI or fleet-wide CA in v1.
- Multi-control-host management in v1 (kept possible but not implemented).
- Fine-grained RBAC or per-session access tokens in v1.

## 5. Threat Model

| Asset | Threat | Mitigation |
|-------|--------|------------|
| Remote daemon identity | MITM on first bootstrap pins a malicious cert | TOFU is bounded by SSH trust for the first bootstrap; subsequent dials pin the cert. |
| Remote daemon identity | Stolen remote daemon cert reused elsewhere | TLS pin is per control-host profile; the cert is only meaningful to hosts that already pinned it. |
| Control host authorization | Stolen operator private key signs challenges | Store key in OS keyring; allow rotation; old public key can be removed from remote `authorized_operators`. |
| Control host authorization | Attacker adds their own public key to remote `authorized_operators` | Directory is written only by the bootstrapper over SSH; no runtime API mutates it in v1. |
| Long-term compromise | Compromised cert or key lives forever | Certificate expiry + rotation; operator key rotation command. |
| Compromised control host | Attacker reads profile database | Profiles contain pins and host names but not operator private keys after keyring migration. |

## 6. Architecture

### 6.1 Remote daemon identity: TLS pin

Keep the existing TLS-pin mechanism:

- On first successful SSH bootstrap, record `tls_pin_sha256` in the profile.
- On every subsequent gRPC dial, verify the remote daemon certificate SPKI hash matches the stored pin.
- If the remote daemon regenerates its certificate (rotation), update the stored pin after a successful SSH-bootstrap verification.

This is the same trust model as SSH host keys: first connection is trusted via the transport, later connections are verified against the recorded key.

### 6.2 Control host authorization: operator challenge

Keep the existing operator challenge:

- Remote daemon holds a set of authorized operator public keys in `~/.waitagent/authorized_operators/`.
- During gRPC handshake the remote daemon sends a nonce challenge.
- Control host signs the nonce with its operator private key.
- Remote daemon verifies the signature against any installed operator public key.

No JWT, no session tokens, no per-call authorization in v1.

### 6.3 Unified secret storage in OS keyring

All local secrets — operator private key, SSH password, and sudo password — are stored in the OS keyring. There is no file-based fallback.

Introduce a `SecretStore` trait (or extend the existing `RemoteHostSecretStore`) with an OS-keyring implementation:

```rust
pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<Secret>, SecretStoreError>;
    fn put(&self, id: &str, secret: &Secret) -> Result<(), SecretStoreError>;
    fn delete(&self, id: &str) -> Result<(), SecretStoreError>;
}
```

Implementation:

- `KeyringSecretStore`: stores secrets in the OS keyring (Linux secret-service / kwallet, macOS Keychain, Windows Credential Manager via a suitable crate).
- If the OS keyring is unavailable, the operation fails with a clear error; the user must enable the keyring or add credentials again.

Migration from the current `FileRemoteHostSecretStore`:

- On first run after upgrade, read existing encrypted files and migrate each secret into the keyring.
- Delete the file-backed secret after successful migration.
- No export/import command is provided; moving to a new control host requires re-adding credentials.

Operator key specifics:

- The operator private key is stored under a well-known keyring entry (e.g., `waitagent.operator_key`).
- `sign_challenge` retrieves the key from the keyring, signs, and returns the signature without exposing the key to callers.
- `rotate` generates a new keypair, replaces the keyring entry, and returns the new public key.

### 6.4 Operator public key distribution

The bootstrapper already installs the operator public key on the remote host:

- Path: `~/.waitagent/authorized_operators/<fingerprint>.pub`
- Filename is the SHA-256 fingerprint of the public key.
- Multiple public keys may coexist; the remote daemon accepts any valid signature.
- Revocation is removing the file.

### 6.5 Certificate rotation

Remote daemon certificates receive a limited lifetime:

- Default validity: 90 days.
- During connect, the bootstrapper checks the certificate expiry over SSH.
- If expiry is within 30 days, the bootstrapper runs `__generate-node-credentials` again, retrieves the new SPKI hash, and updates `RemoteHostProfile.tls_pin_sha256`.
- The old pin is overwritten; no CRL or revocation is needed because the cert is self-signed and pinned per profile.

#### Startup snapshot reconnect expiry fallback

If the control host stops while a remote daemon certificate is still valid, and the certificate expires before the control host restarts, the snapshot-driven reconnect will fail because the stored TLS pin no longer matches the remote daemon's current certificate.

To handle this:

1. `ReconnectSnapshotHosts` attempts the normal gRPC dial using the stored TLS pin.
2. If the dial fails with a TLS/certificate error, and the profile is outbound-dial, trigger a lightweight SSH bootstrapper check for that host.
3. The bootstrapper inspects the remote daemon certificate; if it is expired, it runs `__generate-node-credentials`, retrieves the new SPKI hash, updates `RemoteHostProfile.tls_pin_sha256`, and re-attempts the gRPC dial.
4. If the SSH path also fails, the node is marked offline and the normal retry worker takes over.

This fallback is only used during startup snapshot recovery; normal online reconnections do not need SSH because the bootstrapper already rotates certificates before expiry during active use.

### 6.6 Operator key rotation

Add a CLI command:

```text
waitagent rotate-operator-key [--new-key-path <path>] [--push-to-all]
```

Behavior:

1. Generate a new operator keypair (Ed25519 preferred; RSA 4096 fallback).
2. Store the new private key in the active `OperatorKeyStore`.
3. If `--push-to-all` is given, SSH into each saved profile and install the new public key alongside the old one.
4. After all hosts confirm the new key, optionally remove the old public key from remote hosts and delete the old private key locally.

## 7. LAN Hosts

LAN hosts keep the same control-plane auth as cloud hosts. The only difference is that LAN hosts may use password authentication for SSH bootstrap, while cloud hosts require key auth. The TLS pin and operator challenge are unchanged.

## 8. Multi-Control-Host (Deferred)

In v1 each control host has its own operator keypair. If multiple workstations need to manage the same cloud host, the user must copy each workstation's operator public key to the remote host's `authorized_operators/` directory. A future slice may add a `waitagent authorize-operator-key <host> <pubkey-file>` command to automate this.

## 9. Migration from Current Implementation

1. Replace `FileRemoteHostSecretStore` and the current file-based operator key path with a single `KeyringSecretStore`.
2. On first run after upgrade, migrate existing encrypted file secrets into the keyring and delete the files.
3. Add certificate expiry to `__generate-node-credentials` output and rotation logic in the bootstrapper.
4. Add startup snapshot reconnect expiry fallback.
5. Add `rotate-operator-key` command.
6. Update docs and acceptance tests.

If the OS keyring is not available after upgrade, the migration fails with a clear error; the user must enable the keyring and re-add credentials. No fallback to file storage is provided.

## 10. Confirmed Decisions

1. **Secret storage:** All local secrets (operator key, SSH password, sudo password) are stored in the OS keyring. No file fallback.
2. **Export/import:** Not provided. Moving to a new control host requires re-adding credentials.
3. **Certificate lifetime:** 90 days, with renewal window 30 days before expiry.
4. **Operator key rotation:** On-demand CLI only; no automatic expiry.
5. **Multi-control-host:** Deferred to v2.
6. **Startup reconnect expiry:** If snapshot gRPC dial fails due to an expired certificate, fall back to SSH bootstrapper to regenerate the certificate.

## 11. References

- `docs/reconnection-plan.md` — outbound-dial / reconnect context
- `docs/remote-host-connect-and-session-creation-design.md` — Ctrl-W / Ctrl-S flow
- `src/infra/remote_grpc_transport.rs` — `TlsPinConnector`, operator challenge
- `src/host/ssh/ssh_remote_host_bootstrapper.rs` — bootstrap and operator public key installation
- `src/host/ssh/remote_host_history_store.rs` — `RemoteHostProfile`, `RemoteHostKind`
