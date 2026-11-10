# lightning-ledger (npx launcher)

`npx lightning-ledger@<version> <command>` runs the `lightning` binary for your platform.

On first use it downloads `lightning-<target>` from the GitHub release whose tag matches this package's version, checks its SHA-256 against `checksums.json` (written by the release workflow and shipped inside this package), and caches it. It prints the URL and checksum it used. A missing or mismatched checksum stops it. Nothing runs unverified.

Prebuilt binaries cover macOS (Apple Silicon, Intel), Linux (x64, ARM64; static musl builds) and Windows (x64, ARM64). Set `LIGHTNING_BINARY` to use a binary you built yourself instead.

See https://github.com/danzang100/lightning for documentation.
