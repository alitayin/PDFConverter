# Notices

The application is proprietary. It includes third-party open-source
components whose licenses remain applicable to those components. A complete,
versioned inventory is generated for every release candidate by:

```text
node scripts/generate-release-manifest.mjs
```

The generated `THIRD_PARTY_LICENSES.md` and CycloneDX `sbom.cdx.json` files
are the authoritative release-time inventory. Components with an unknown
license are release blockers in strict mode. Electron supplies the embedded
Chromium runtime, so no separate browser runtime is required.
