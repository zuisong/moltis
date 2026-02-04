# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Moltis, please report it responsibly.

### Preferred Methods

1. **GitHub Security Advisories** (recommended): Use [GitHub's private vulnerability reporting](https://github.com/penso/moltis/security/advisories/new) to report issues confidentially.

2. **Email**: Send details to [moltis AT pen DOT so](mailto:moltis AT pen DOT so)

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

### Response Timeline

- **Acknowledgment**: Within 48 hours
- **Initial assessment**: Within 7 days
- **Fix timeline**: Depends on severity, but we aim to address critical issues as quickly as possible

### Scope

This policy applies to the Moltis codebase. Third-party dependencies should be reported to their respective maintainers.

## Supported Versions

Security updates are provided for the latest release only.

## Verifying Release Signatures

All release artifacts are signed using [Sigstore](https://sigstore.dev) keyless
signing. This provides cryptographic proof that artifacts were built by our
GitHub Actions workflow, not tampered with after the fact.

### Install cosign

```bash
# macOS
brew install cosign

# Linux (or download from https://github.com/sigstore/cosign/releases)
go install github.com/sigstore/cosign/v2/cmd/cosign@latest
```

### Verify a binary/package

Each release artifact has three companion files:
- `.sha256` / `.sha512` — checksums
- `.sig` — Sigstore signature
- `.crt` — Signing certificate

```bash
# Download the artifact and its signature files
curl -LO https://github.com/penso/moltis/releases/download/v0.1.0/moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz
curl -LO https://github.com/penso/moltis/releases/download/v0.1.0/moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz.sig
curl -LO https://github.com/penso/moltis/releases/download/v0.1.0/moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz.crt

# Verify the signature
cosign verify-blob \
  --signature moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz.sig \
  --certificate moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz.crt \
  --certificate-identity-regexp="https://github.com/penso/moltis/*" \
  --certificate-oidc-issuer="https://token.actions.githubusercontent.com" \
  moltis-0.1.0-x86_64-unknown-linux-gnu.tar.gz

# Expected output: Verified OK
```

### Verify a Docker image

```bash
# Verify the image signature
cosign verify \
  --certificate-identity-regexp="https://github.com/penso/moltis/*" \
  --certificate-oidc-issuer="https://token.actions.githubusercontent.com" \
  ghcr.io/penso/moltis:latest

# View the SBOM attached to the image
cosign download sbom ghcr.io/penso/moltis:latest

# View provenance attestation
cosign verify-attestation \
  --type slsaprovenance \
  --certificate-identity-regexp="https://github.com/penso/moltis/*" \
  --certificate-oidc-issuer="https://token.actions.githubusercontent.com" \
  ghcr.io/penso/moltis:latest
```

### What the signature proves

- The artifact was built by GitHub Actions in the `penso/moltis` repository
- The build was triggered by a specific commit (visible in the certificate)
- The artifact has not been modified since signing
- No private keys are involved — signatures use GitHub's OIDC identity

### Transparency log

All signatures are recorded in Sigstore's public transparency log (Rekor).
You can search for moltis signatures at: https://search.sigstore.dev
