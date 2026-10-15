# Security policy

OneUpDrive handles account tokens, personal files and vault encryption keys. We take reports seriously.

## Reporting a vulnerability
Please **don't open a public issue**. Use GitHub's private vulnerability reporting ("Report a vulnerability" in the Security tab). Include steps to reproduce and the impact. We aim to acknowledge reports within 72 hours.

## In scope
* Token handling (storage, logging, transmission)
* Anything that could cause data loss or corruption, including sync-engine logic bugs. We treat these as security issues.
* Local privilege or IPC issues (named pipe access, the Cloud Files provider)
* Vault cryptography and key storage
* Updates: the update check, download and signature verification
* The Explorer extension (`oneup_shell.dll`) and the identity package

## Design commitments
* Public client only; no client secrets are shipped.
* Refresh tokens, and remembered vault keys, are stored encrypted for the current Windows user (DPAPI).
* Updates are installed only if Authenticode-signed by the same publisher as the running copy.
* What the agent learns about program usage (the Apps page) stays on the PC: program names, counts and the paths of recently used files, never contents.
* Logs never contain tokens, keys or file contents.
* Crypto uses audited RustCrypto implementations only, with key material zeroized after use.
* Vaults: masterkey-file vaults only (no remote key-loading endpoints), with the vault config signature verified before any field is trusted. See [vaults.md](docs/architecture/vaults.md).
