# Release builds and code signing

Releases are built and signed via [SignPath.io](https://signpath.io)'s free
program for open-source projects, using a verifiable CI build rather than a
locally-built exe — required for SignPath eligibility, and generally better
practice for anything users install and run.

## How a release works

1. `.github/workflows/release.yml` cross-compiles `resmon.exe` and
   `resmon-mcp.exe` (MinGW target), builds the icon, and packages
   `ResourceMonitorSetup.exe` with NSIS — all inside GitHub's runner, from
   the tagged source, with no local build artifacts involved.
2. The unsigned installer is uploaded as a build artifact.
3. A SignPath signing request submits that artifact for signing. Each
   release requires manual approval in the SignPath dashboard before the
   signed binary comes back — this is a deliberate control, not a bug.
4. The signed `ResourceMonitorSetup.exe` is attached to the GitHub Release.

## One-time setup (once the SignPath application is approved)

SignPath requires the project to already be public with an OSI-approved
license (this repo qualifies: MIT) before applying at
[signpath.io/solutions/open-source-community](https://signpath.io/solutions/open-source-community).
Once approved, SignPath provides:

- an **Organization ID** and **Project slug**
- a **signing policy** slug (e.g. `release-signing`)
- an **API token** for the GitHub Action to authenticate with

Add those as repository secrets (`SIGNPATH_API_TOKEN`,
`SIGNPATH_ORGANIZATION_ID`) and fill in the project/policy slugs in the
workflow's SignPath step, which is included but inert (no secrets configured)
until then.

## Why not a paid certificate

A traditional OV certificate (Certum, SSL.com) works too and needs no
approval queue, but costs the maintainer money for a certificate whose
private key they'd have to store and rotate themselves. SignPath's HSM-backed
signing has no such handling burden and is free for qualifying OSS — the
main tradeoff is the per-release manual-approval step.
