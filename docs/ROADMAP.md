# Roadmap

Priorities are directional, not delivery-date commitments. Implemented features
and their limits are documented in [FEATURES.md](FEATURES.md).

## Public launch

1. Run the Linux x86-64/ARM64 and macOS Intel/Apple Silicon distribution matrix.
2. Validate install/upgrade and native GUI behavior on clean, non-Omarchy Linux
   and macOS machines, including Keychain, SSH, TLS and optional client tools.
3. Add Apple Developer ID signing, notarization and stapling.
4. Publish reviewed release assets and verify installation from the real GitHub
   release through mise. Draft releases intentionally remain unavailable to mise.

See [DISTRIBUTION.md](DISTRIBUTION.md) for build and release requirements.

## Follow-up work

- Broader Linux installation formats and macOS DMG packaging.
- Reliable cross-engine/version coverage and platform-specific acceptance tests.
- Diagram layout persistence and image export.
- Better SQL diagnostics and conservative expansion of completion coverage.
- Additional structured-value viewers and accessibility improvements.
- Artifact provenance/attestations and an explicit update strategy.

Windows, additional database engines, plugins, AI integrations and query-result
charts depend on demonstrated demand and suitable implementation boundaries.

Every change must preserve explicit Review → Apply, bounded memory, independent
workspace/session ownership, fast ordinary Run and honest partial/error states.
