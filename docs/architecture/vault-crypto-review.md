# Vault cryptography: self-review

A review of OneUpDrive's Cryptomator vault code against the public vault format 8 specification, by the project itself. **It is not an independent audit.** Vaults stay labelled experimental (user guide, settings window, `oneup vault --help`) until an outside reviewer has looked at them; the appendix is the request to send one. Design overview: [vaults.md](vaults.md).

## Scope and method
* **Code:** all of `crates/oneup-vault`; the key use in `crates/oneup-vaultsync` (`project`, `cloud`); key handling in `bins/oneup` (`vaultpair.rs`, `vault.rs`, `launch.rs`, `bin/oneupd.rs`, `bin/oneup-settings.rs`); `oneup_sys::dpapi` and `oneup_sys::hello`; the RustCrypto crates as used here (`aes` 0.8, `aes-gcm` 0.10, `aes-siv` 0.7, `cmac` 0.7, `aes-kw` 0.2, `scrypt` 0.11, `hmac` 0.12, `sha2` 0.10, `subtle` 2, `getrandom` 0.3, `zeroize` 1).
* **Reference:** docs.cryptomator.org, *Security Architecture* and *Vault Cryptography* (format 8). Compared from the published text only; no Cryptomator code was read.
* **Attackers considered:** (A) the storage provider, or anyone who can change files in OneDrive, without the password; (B) someone holding the vault's key who crafts a hostile vault (a shared vault, a vault received from someone else); (C) another program or user on this PC, reading memory left behind after use, or files in the data folder. Malware running as the user while a vault is unlocked can read the decrypted folder and is out of scope.
* **Severity:** *High*: a type-A attacker reads or forges content, or keys leak to disk. *Medium*: data encrypted under the wrong key, or undetected tampering that the format allows. *Low*: hardening (memory residue, defence in depth, hostile-vault robustness). *Info*: noted, no change needed.

## Conformance with the specification

| Item | Specification | Implementation | Verdict |
|---|---|---|---|
| Masterkey | Two 256-bit keys from a CSPRNG | `key.rs:113` (`getrandom`, filled in place) | Matches |
| Password KDF | scrypt, `scryptCostParam` 32768, `scryptBlockSize` 8, non-parallel | `key.rs:77`: N = 2^15, r = 8, p = 1, 8-byte salt, 32-byte output; NFC-normalised UTF-8 password; N must be a power of two, memory capped at 1 GiB | Matches (salt length and NFC not in the text; same as Cryptomator's files) |
| Key wrap | AES key wrap, RFC 3394 | `aes-kw` `KekAes256`, 40-byte wrapped keys only (`key.rs:192`) | Matches; RFC 3394 §4.6 vector |
| `versionMac` | HMAC-SHA256 over the 4-byte big-endian version | `key.rs:97`, checked with `verify_slice` (constant time), then version must be 999 | Matches |
| `vault.cryptomator` | JWT; `kid` read unverified; HS256 (HS384/512 also); key = encryption key ‖ MAC key; verify, then check `format` and `cipherCombo` | `config.rs:62` (`kid`: `masterkeyfile:` + plain file name only; `hub+` refused), `config.rs:113` (signature over the received `header.payload`, compared with `subtle`, before the payload is parsed; `alg` limited to HS256/384/512; format 8 and `SIV_GCM` required) | Matches |
| File header | 12-byte nonce, AES-GCM(encryption key) of `FF×8` ‖ 32-byte content key, 16-byte tag; 68 bytes | `content.rs:75`, read at `content.rs:121` and `content.rs:268` | Matches |
| Chunks | ≤ 32 KiB; random 12-byte nonce; AES-GCM(content key); AAD = chunk number (u64 BE) ‖ header nonce | `content.rs:51` (AAD), `content.rs:92`, `content.rs:199` | Matches |
| Directory IDs | Root "", others ≤ 36 ASCII (UUIDs); `d/` + Base32(SHA-1(AES-SIV(ID))) split 2 + 30 | `names.rs:45`; IDs read with the length and ASCII check (`folder.rs:101`, `project.rs:249`) | Matches |
| Names | NFC, AES-SIV with parent ID as AD, Base64url, `.c9r` | `names.rs:55`, `names.rs:63`; key order is RFC 5297's (S2V key first), pinned by a test | Matches |
| Shortening | > 220 chars (including `.c9r`) → Base64url(SHA-1(name)) + `.c9s`, `name.c9s`, `contents.c9r` | `names.rs:86`, `names.rs:99`; readers check the `.c9s` folder name against its `name.c9s` | Matches |
| `dirid.c9r` | Optional backup | Not written, ignored when read | Allowed |
| `SIV_CTRMAC`, Hub | Other cipher combo; Hub key loading | Refused with a message | Out of scope by design |

## Findings

| ID | Severity | Finding | Location | Status |
|---|---|---|---|---|
| V-1 | Medium | Keys given to a vault's agent weren't checked against the vault's configuration | `crates/oneup-vaultsync/src/project.rs:192` | Fixed |
| V-2 | Low | Authentic names that are paths (`..`, `a/b`, empty, NUL) were accepted | `crates/oneup-vault/src/names.rs:63` | Fixed |
| V-3 | Low | The streaming decryptor's buffer could grow, leaving plaintext in freed memory | `crates/oneup-vault/src/content.rs:322` | Fixed |
| V-4 | Low | Key bytes were moved and returned by value, leaving stack copies | `crates/oneup-vault/src/key.rs:35`, `lib.rs:54`, `content.rs:75` | Fixed |
| V-5 | Low | AES-SIV's CMAC state wasn't wiped | `crates/oneup-vault/Cargo.toml` | Fixed |
| V-6 | Low | Strings holding secrets grew by reallocation, leaving copies | `key.rs:77`, `recovery.rs:35`, `recovery.rs:52`, `bins/oneup/src/vaultpair.rs:203` | Fixed |
| V-7 | Low | The agent read its keys through Rust's buffered stdin, and could panic on bad input | `bins/oneup/src/vaultpair.rs:217` | Fixed |
| V-8 | Low | Passwords and recovery keys typed at the command line or in the settings window were plain `String`s | `bins/oneup/src/vault.rs:19`, `vault.rs:281`, `bin/oneup-settings.rs:442` | Fixed (Rust side; see V-17) |
| V-9 | Low | The Windows Hello signature and its hash weren't wiped | `crates/oneup-sys/src/hello.rs:56` | Fixed |
| V-10 | Low | Decrypted names went into the vault agent's log | `crates/oneup-vaultsync/src/project.rs:148`, `bins/oneup/src/vaultpair.rs:385` | Fixed |
| V-11 | Low | The settings window trimmed the password when adding a vault | `bins/oneup/src/bin/oneup-settings.rs:442` | Fixed |
| V-12 | Info | The recovery key's check value was compared with `!=` | `crates/oneup-vault/src/recovery.rs:71` | Fixed |
| V-13 | Medium | Format: truncation at a chunk boundary, rollback, deletion and swapping of whole files aren't detected | `crates/oneup-vault/src/content.rs:12` | Open (format) |
| V-14 | Low | Format: folder placement (`dir.c9r`) isn't authenticated | `crates/oneup-vaultsync/src/project.rs:249` | Partly fixed |
| V-15 | Low | Format: random 96-bit header nonces under one key | `crates/oneup-vault/src/content.rs:75` | Accepted |
| V-16 | Low | scrypt working memory, HMAC/SHA-2 states and moved cipher objects aren't wiped | upstream crates; `key.rs:77`, `config.rs:78` | Open |
| V-17 | Low | Settings window secrets travel as JSON strings (webview, pipe, `serde_json::Value`) | `bins/oneup/src/bin/oneup-settings.rs:193`, `:486` | Open |
| V-18 | Info | DPAPI-remembered keys are readable by any program running as the user | `crates/oneup-sys/src/lib.rs:79`, `bins/oneup/src/vaultpair.rs:129` | Accepted, documented |
| V-19 | Info | The Windows Hello secret is the hash of one signature, assumed deterministic | `crates/oneup-sys/src/hello.rs:56` | Accepted; determinism needs a hand check |
| V-20 | Low | Several stored names can decrypt to one name (padding, case, NFC/NFD) | `crates/oneup-vault/src/names.rs:67`, `crates/oneup-plan/src/folder.rs:158` | Open |
| V-21 | Info | JWT: unknown header parameters (`crit`) ignored; `alg` chosen by the file | `crates/oneup-vault/src/config.rs:113` | Accepted |
| V-22 | Info | Reserved header bytes aren't checked on reading | `crates/oneup-vault/src/content.rs:121` | Accepted |
| V-23 | Info | Error messages | all modules | No oracle found |
| V-24 | Info | Passwords from `ONEUP_VAULT_PASSWORD` / `ONEUP_VAULT_RECOVERY_KEY` stay in the environment | `bins/oneup/src/vault.rs:19` | Accepted (scripts, tests) |
| V-25 | Info | No format-level test vectors exist; interoperability is checked by hand | `docs/architecture/vaults.md` | Open |
| V-26 | Low | The keys are verified once per round, not again before the round's first change in OneDrive | `crates/oneup-vaultsync/src/project.rs:192`, `crates/oneup-vaultsync/src/cloud.rs:205` | Open |

### V-1 Keys not checked against the configuration (Medium, fixed)
The agent got its keys from standard input, `vault-key.dpapi` or the Windows Hello blob and used them as they were. Keys typed at unlock were verified (`vaultpair.rs:285`), but remembered keys outlive the vault they came from: if the vault in OneDrive was deleted and a new one created at the same path, the agent would list nothing (every name fails authentication) yet encrypt new local files with the old keys into the new vault, where neither Cryptomator nor OneUpDrive with the new password could read them. **Fix:** every projection first verifies `vault.cryptomator` with the keys (`check_keys`, read once per eTag, so no extra download in steady state) and checks the shortening threshold the pair writes with; otherwise the round stops with "these keys don't open the vault in OneDrive (was it replaced?); unlock it again with its password". Uploads happen only in rounds, after the projection (a replacement during a round: V-26). Test: `oneup-vaultsync/tests/vault.rs`, `keys_that_dont_sign_the_vaults_configuration_are_refused` (another vault's keys, a replaced configuration, a missing one).

### V-2 Authentic names that are paths (Low, fixed)
AES-SIV proves a name came from a key holder, not that it is a name. Attacker B can encrypt `..`, `a/b` or `x\0y`. The effects layer already refuses to create such names (`oneup_effects::name::is_valid_name`), so nothing was written outside the folder, but they entered the decrypted Remote tree, where paths are joined with `/`. **Fix:** `decrypt_name` refuses empty, `.`, `..`, and names containing `/` or NUL, which no app on any platform creates. Backslashes and other characters Windows forbids stay legal (they're ordinary on macOS and Linux) and are refused at creation by the effects layer, reported as invalid names. Tests: `names::tests::authentic_names_that_are_paths_are_refused`; and `oneup-vaultsync/tests/vault.rs`, `refused_entries_keep_their_rows_and_produce_no_deletions`, which projects and applies a vault, renames entries to each refused name (and a long-name folder away from its `name.c9s`, V-14), projects and applies again: every entry is reported, every row stays as it was, and nothing is removed (no deletion evidence).

### V-3 Decryptor buffer growth (Low, fixed)
`Decryptor::write` appended whatever it was given, then decrypted chunks in place. A large write grew the `Zeroizing<Vec>`, and the old allocation, possibly holding decrypted chunks, went back to the heap unwiped. **Fix:** input is taken in pieces that fit the fixed capacity (two chunks); the buffer never moves. Test: `content::tests::streaming_decryption_never_regrows_its_buffer`.

### V-4 Key bytes moved by value (Low, fixed)
`Masterkey` held two arrays inline, so every move (returning it, `Arc::new`) copied 64 key bytes, and `random()` returned keys by value. **Fix:** the keys live in a `Box` zeroed on drop; `Clone`, `import`, `generate` and `from_file` write into it in place; content keys are generated inside the header buffer they're encrypted in (`new_header`). Cipher objects (`Aes256Gcm`, `Aes256Siv`) are still returned by value; see V-16.

### V-5 CMAC state (Low, fixed)
`aes-siv` wipes its CTR key, and the `aes` feature wipes the AES key schedules, but the `cmac` crate wipes its state only with its `zeroize` feature, which nothing enabled. **Fix:** enabled in `oneup-vault/Cargo.toml`; a compile-time test (`aes_key_schedules_are_wiped_on_drop`) fails if it's lost.

### V-6, V-7, V-8, V-9, V-12 Copies of secrets (Low/Info, fixed)
* The NFC password, the recovery key text (both ways) and the agent's hex key line are built in strings sized up front, so no reallocation leaves a partial copy; `key_line` no longer makes a temporary string per byte.
* `key_from_stdin` read through `std::io::stdin()`, whose 8 KiB buffer lives for the life of the process. It now reads straight from a duplicate of the handle into a zeroed fixed buffer, and parses bytes (a 128-byte line of non-ASCII text used to panic on a char boundary). The line must end with its newline (input that stops early is refused, not guessed at). Tests: `vaultpair::tests::keys_go_to_the_agent_as_one_exact_line`, and `the_agent_reads_its_key_line_however_the_pipe_delivers_it` (one byte at a time, interrupted reads retried, end of input before the newline, another I/O error returned, an over-long line).
* CLI passwords and recovery keys (`rpassword`) and the settings window's copies are `Zeroizing<String>`.
* `hello::secret` wipes the signature bytes and the digest after use (the signature is as good as the secret), and writes the secret into the caller's `Zeroizing` buffer instead of returning it by value.
* The recovery key's 32-bit check is compared with `subtle`. It isn't an oracle (whoever types the key has the keys), so this is hygiene.

### V-10 Decrypted names in the log (Low, fixed)
Projection problems (`"<name>" is incomplete`, `<name>: not a valid encrypted file`) used decrypted names, and the agent prints problems to its log, a plain file in the data folder that outlives the vault pair. **Fix:** problems name the encrypted item only. The decrypted folder itself, and the vault pair's activity log, still hold decrypted names by design.

### V-11 Password trimmed (Low, fixed)
`vault_add` in the settings window trimmed the password, so a vault whose password starts or ends with a space couldn't be added there (the CLI and `vault_create` didn't trim). Not a security flaw; it now passes the password as typed.

### V-13 Undetected truncation, rollback, deletion and swapping (Medium, open: format)
The format has no end-of-file marker and doesn't bind a file's header to its name or path. A type-A attacker can cut a file at any chunk boundary (down to an empty file of 68 bytes), put back an older valid version, delete it, or swap the encrypted contents of two files in the same vault; all authenticate. OneUpDrive syncs the result as a remote edit (the previous local version goes to the version store like any replaced file) or a delete with evidence (gated as usual). Can't be fixed without leaving the format. Mitigations: OneDrive's version history; the user guide and [vaults.md](vaults.md) state it.

### V-14 Folder placement not authenticated (Low, partly fixed)
`dir.c9r` holds the folder ID in plaintext, unauthenticated. A type-A attacker can swap two folders' `dir.c9r`, showing each folder's contents under the other's name (entries still decrypt: they're bound to the ID, not the folder's name), or copy one so a folder appears twice (the projection shows the second copy empty and reports it, `project.rs:123`). Format property, open. Related and **fixed**: a `.c9s` folder's name wasn't checked against its `name.c9s`, so copying one long name's `name.c9s` into another entry showed the first entry twice; both readers now require `shortened(name.c9s) == folder name` (`project.rs:238`, `folder.rs:178`; test `a_long_name_copied_into_another_entry_is_reported_not_trusted`).

### V-15 Random header nonces (Low, accepted)
Every file version gets a random 96-bit nonce under the vault's encryption key. NIST SP 800-38D limits random-nonce GCM to 2^32 messages per key; a collision would expose two content keys' XOR and GHASH key material for headers. That's about four billion file writes per vault, beyond any realistic use. Format property.

### V-16 Unwiped working state in upstream crates (Low, open)
* `scrypt` 0.11 allocates its B, V and T buffers with `vec!` and frees them unwiped. V's first block is PBKDF2(password, salt), so a later reader of freed memory could test password guesses at one PBKDF2 iteration each, skipping scrypt's cost.
* `hmac` 0.12 and `sha2` 0.10 don't wipe their states. The inner and outer HMAC states keyed with the password (PBKDF2), the MAC key (`versionMac`) or the raw masterkey (JWT) are key-equivalent and stay on the stack.
* Cipher objects returned by value (`Aes256Gcm::new`, `Aes256Siv::new`) can leave round-key copies on the stack; the final copy is wiped on drop.

All need an attacker of type C who can read this process's freed memory, which, while a vault is unlocked, can read the keys directly. Options: wiping wrappers or upstream `zeroize` support; running unlock (scrypt) in a short-lived helper process.

### V-17 Secrets in the settings window's messages (Low, open)
The window sends passwords and receives recovery keys as JSON over the WebView2 bridge and the control pipe; copies sit in JavaScript strings, the message buffers and `serde_json::Value`, none of which can be wiped. Inherent to a web-based window; a native password dialog would avoid it.

### V-18 DPAPI-remembered keys (Info, accepted)
`CryptProtectData` with no entropy and no prompt: any program running as the user can unprotect `vault-key.dpapi`, as it can the OneDrive refresh token. The file is the convenience the user asked for ("remember"); the user guide now says who can read it. Windows Hello (V-19) is the option that asks every time.

### V-19 Windows Hello secret (Info, accepted)
The secret is SHA-256 of the signature a Windows Hello key makes over a challenge stored next to the sealed keys. This **assumes the signature is deterministic** (RSA PKCS #1 v1.5): Microsoft's `RequestSignAsync` page doesn't name the padding; Microsoft's Windows Hello sign-in samples verify these signatures as PKCS #1 v1.5 with SHA-256, and other password managers rely on the same property. It hasn't been confirmed on a PC here. **Hand check:** turn on Unlock with Windows Hello for a test vault, lock it, and unlock it with Windows Hello twice. If signatures were randomised (RSA-PSS), the second unlock would fail its integrity check and unlocking would fall back to the password: no data is at risk, only the convenience. Each signature needs the person's approval, but one approved request, for example a prompt triggered by malware the person approves, yields the secret for that blob for good. Sealing is AES-256-GCM with a random nonce and a fixed purpose label as associated data (`sealed.rs:33`); a blob from another vault pair fails, because its credential name (and so its key) differs.

### V-20 Several stored names for one name (Low, open)
`decrypt_name` accepts Base64url with or without padding, so a vault can hold the same name twice in one folder. Vaults written on macOS or Linux can also hold names that differ only in case, or NFC and NFD spellings (Cryptomator normalises to NFC when writing, other tools may not). OneDrive never produces such pairs, and the planner's remote-only arm (`oneup-plan/src/folder.rs:158`) plans each as its own download. The effects layer creates only under free names, so nothing is overwritten, but the behaviour isn't covered by a test. Next step: an `everyday_` test with two case-only names in a vault, and report them as a name clash.

### V-21 JWT details (Info, accepted)
The file chooses `alg`, but only among HS256/384/512 with the same key, which gives an attacker nothing; `none`, RS*, ES* and anything else are refused (test `unsafe_headers_are_refused`). The signature is checked over the exact received bytes, before the payload is parsed. Unknown header parameters, including `crit`, are ignored; Cryptomator defines none. Duplicate JSON fields are refused by `serde`.

### V-22 Reserved header bytes (Info, accepted)
The eight `0xFF` bytes aren't checked when reading. They're inside the authenticated header, and the specification reserves them for future use, so refusing other values would only break future vaults.

### V-23 Error messages (Info)
Callers can tell "wrong password" (key-wrap integrity), "integrity check failed" (header, chunk, name, configuration, sealed keys) and "malformed" apart. No oracle results: GCM checks the tag before decrypting (`aes-gcm` compares in constant time), AES-SIV re-encrypts on failure and compares with `CtOutput`, and every "malformed" after decryption (non-UTF-8 name, invalid name) comes after authentication. A changed wrapped key reads as "wrong password", which an attacker who can edit the file learns nothing from. Masterkey and vault `Debug` output is redacted; nothing in `oneup-vault` logs.

### V-25 Test vectors (Info, open)
The specification publishes no known-answer vectors for the format, only an example `vault.cryptomator` without its key (read as far as possible: `the_specifications_example_reads_as_expected_but_not_verified`). Pinned here: RFC 3394 §4.6 (key wrap), RFC 5297 A.1 (AES-SIV and its key order), RFC 7914 §12 vector 1 (scrypt, new). Interoperability with Cryptomator Desktop is a manual check ([vaults.md](vaults.md)). Next step: commit a small vault made by Cryptomator Desktop with a published test password as a fixture, and check names, folder IDs and contents against it in CI.

### V-26 Keys checked once per round (Low, open)
V-1's check runs when the round builds its view (`project`); `VaultCloud` then uses the same keys for the round's uploads, folders and moves. If another client replaces `vault.cryptomator` (a new vault with new keys, at the same path) between that check and this round's first change, files can still be encrypted with the old keys into the new vault. No content is lost: the plaintext stays in the local folder, the uploaded files stay readable with the old keys, and the next round's check stops the agent. **Suggested fix:** carry the eTag of the configuration `project` verified into `VaultCloud`, and before the round's first change in OneDrive re-read the configuration's eTag (`Cloud::item`) and stop if it changed.

## Checked and found sound
* **RNG:** `getrandom` 0.3 (`ProcessPrng` on Windows) for keys, salts, nonces and UUIDs; nothing else produces randomness.
* **Constant time:** the JWT signature (`subtle`), `versionMac` (`verify_slice`), GCM tags, AES-SIV tags. The key-wrap IV check in `aes-kw` uses `==`, which leaks nothing an attacker holding the file can't compute offline.
* **Hostile files:** scrypt cost capped at 1 GiB and power-of-two; small files (`dir.c9r`, `name.c9s`, configuration) size-capped before reading; key file name restricted to the vault folder; Hub refused before any network use.
* **Nothing unauthenticated is released:** whole-file and streaming decryption authenticate each chunk before writing it; a cut inside a chunk fails; downloads go to staging and are checked (size, encrypted hash) before the placeholder gets a byte.
* **What's logged:** no keys, passwords or content; problems name encrypted items only (V-10).

## Appendix: request for an outside review

> **Request for review: OneUpDrive's Cryptomator vault implementation (Rust)**
>
> OneUpDrive is an open-source (MIT) OneDrive sync client for Windows. It reads and writes Cryptomator vault format 8 (password vaults, `SIV_GCM`), implemented from Cryptomator's published specification with RustCrypto crates, and syncs a vault in OneDrive as a decrypted folder on the PC. We'd like an independent review of the cryptography before the feature loses its "experimental" label.
>
> **In scope** (about 2,000 lines of Rust):
> * `crates/oneup-vault`: masterkey file (scrypt, AES key wrap, `versionMac`), `vault.cryptomator` JWT verification, file header and chunk encryption (streaming encryptor and decryptor), AES-SIV names, directory IDs, name shortening, our own recovery-key encoding (`recovery.rs`) and the Windows Hello sealing of keys (`sealed.rs`).
> * Key handling: `crates/oneup-vaultsync/src/project.rs` (keys verified against the configuration), `bins/oneup/src/vaultpair.rs` (DPAPI-remembered keys, keys passed to the agent process over a pipe), `crates/oneup-sys/src/hello.rs` and `dpapi` in `crates/oneup-sys/src/lib.rs`.
>
> **Questions we'd most like answered:**
> 1. Does anything deviate from format 8 in a way that weakens it, or that Cryptomator would reject?
> 2. Is the JWT and masterkey-file handling safe against hostile files (algorithm choice, parsing before verification, resource exhaustion)?
> 3. Can a storage-side attacker get plaintext out, or get unauthenticated data accepted, through the streaming decryptor (chunk boundaries, truncation, buffering)?
> 4. Is the Windows Hello construction (a deterministic RSA signature, hashed, as an AES-GCM key) sound for its purpose? Is DPAPI without entropy acceptable for "remember on this PC"?
> 5. Is the recovery-key format (raw keys plus a 32-bit SHA-256 check, Base32) reasonable?
> 6. Key-material hygiene: anything we missed beyond the open items below?
>
> **Our self-review**, with known limitations and open items (format-level truncation and rollback, unwiped state in upstream crates, secrets in the settings window's web view): `docs/architecture/vault-crypto-review.md`. Design: `docs/architecture/vaults.md`. Tests: `cargo test -p oneup-vault -p oneup-vaultsync` (Windows; the format library's tests also run elsewhere).
>
> **Out of scope:** the sync engine in general, Cryptomator Hub (refused), the `SIV_CTRMAC` cipher combination (refused).
>
> Please report vulnerabilities privately, as described in `SECURITY.md`. We'll credit reviewers in the review document unless they prefer otherwise.
