# AGENTS.md — Guidance for AI coding agents

Read this before changing anything. This file holds the rules for working in the repo; [`docs/architecture/`](docs/architecture/overview.md) describes the design.

**The maintainer's private notes** (the plan and roadmap, current status and next steps, developer guides, spike reports and spike code, research) are not in the repository. They live in `private/` at the root of the main checkout, which git ignores (`.git/info/exclude`); from a worktree, the main checkout is the first entry of `git worktree list`. Start with `private/dev/HANDOFF.md` (status and next steps), `private/plan.md` (scope and roadmap) and `private/dev/` (building, testing, the app registration and the test account). If there is no `private/` folder, you're working from a public clone: these docs and the code are all there is.

## What this project is
OneUpDrive is a Rust replacement for the Windows OneDrive desktop sync client. It covers **OneDrive Personal only**, using Microsoft Graph as the storage API. It runs as a headless user-session agent (`oneupd`) plus a CLI (`oneup`) and an optional native tray.

## Prime directive: never lose user data
Before any change that touches the filesystem, the network, the planner, the executor or the state store:
1. Re-read [`docs/architecture/data-safety.md`](docs/architecture/data-safety.md). Those invariants are **hard requirements**, not guidelines.
2. Destructive effects on **user content** (anything in a sync root or in OneDrive: delete, overwrite, move, truncate, dehydrate, remote delete/replace) must go through the `effects` layer behind safety-gate tokens. **Never** add a direct `std::fs::remove_*`, `rename` over an existing file, or Graph `DELETE`/`PUT`-replace for user content anywhere else. OneUpDrive's *own* files in its data directory (state DB, token file, logs) are exempt, but still write them atomically.
3. Deletions require *explicit evidence* (a USN delete record or a delta `deleted` facet). Never infer a deletion from absence without going through the safety gate.
4. Every remote mutation uses `If-Match` eTag preconditions. Every local mutation re-checks file ID + USN + size + mtime immediately before acting.
5. If you can't prove a change is safe, add a test that proves it or don't make the change. Ask the maintainer.
6. Tests and spikes must operate **only** on temp directories or dedicated test accounts. Never point code at a real user's OneDrive folder or real account data. Two exceptions: (a) a read-only diagnostic of a *copy* of a database, with explicit maintainer consent; (b) **read-only** spikes the *maintainer* runs on their own account (e.g. `graph-probe` in `private/spikes/code/`). Agents prepare these and hand over the commands; they don't run them. The maintainer's **test account** is the exception that makes live tests possible: agents may run `oneup` commands with `--data-dir "$env:LOCALAPPDATA\OneUpDrive-test"` (the folder holding `test-account.txt`, see `private/dev/azure-app-registration.md`) and with no other data folder. Never sign it in or out on the maintainer's behalf; the browser sign-in is theirs.

### Files On-Demand (Cloud Files API) rules: learned the hard way
* **Never** call plain `CfDehydratePlaceholder`. Use the exclusive-handle → verify → `CfUpdatePlaceholder(DEHYDRATE | VERIFY_IN_SYNC)` sequence (data-safety invariant 11).
* Windows does not track writes made by processes running the provider's executable. Never write user content into the sync root from `oneupd` except via Cloud Files transfer operations, and keep the CLI/tray as separate executables.
* `FETCH_DATA` must check the placeholder's identity/version and size against the cloud before transferring a single byte.
* Tests that simulate "a user app edited a file" must use a **different executable** (e.g. `cmd.exe`), not a child of the test binary.
* Unprivileged USN records have **no names**. Resolve paths from the DB or by file ID.
* **Windows disguises placeholders by default** for our executables (reparse attribute and tag hidden in listings *and* on handles). Call `oneup_cfapi::expose_placeholders()` before any code that classifies files.
* **Never ask Restart Manager about an online-only file** (`oneup_sys::in_use`): it downloads the whole file, even from our own process (measured). Check the attributes first, without data access.
* **Never read the contents of another provider's placeholder**, hydrated or not. `FILE_FLAG_OPEN_NO_RECALL` does *not* stop a Cloud Files download (measured). Check attributes and reparse tag on the open handle and read ordinary files only (`oneup-local::verify` does this). Listing an unpopulated placeholder *folder* also triggers a fetch; the scanner skips them.

## Performance & resource rules
* Cost must scale with **changes**, not total item count. No periodic full-tree scans in steady state.
* Don't hold the whole tree in memory. Use the indexed SQLite state store; caches must be bounded and sized by the memory profile.
* No busy loops or short polling timers. Block on the USN journal, sockets or channels.
* Target workloads have **millions** of items (real machines have 3M or more). Think about how any new code behaves at 3M items.
* Store 128-bit file IDs (ReFS/Dev Drive), never assume 64-bit.

## User control
Any new background behaviour must be pausable, prioritisable, explainable (`oneup explain`/activity log) and covered by config + CLI. Defaults are conservative.

## Code conventions
* Rust stable, edition 2024, Cargo workspace. `crates/*` are libraries and `bins/*` are executables. The early spikes are standalone crates in `private/spikes/code/`, outside the workspace; production code never depends on them.
* Run `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` before finishing.
* `unsafe` is allowed only in thin FFI wrappers (`oneup-ntfs`, `oneup-cfapi`, `oneup-sys`, and `oneup-shellext`, the Explorer COM extension), each block with a `// SAFETY:` comment. Keep the safe API above it small.
* Errors: `thiserror` in libraries, `anyhow` only in binaries and spikes. No `unwrap()`/`expect()` on runtime data in library code.
* Logging via `tracing`. **Never log file contents, tokens or keys.** Paths only at `debug` level or below (they can be personal).
* Planner and safety gate must stay **pure** (no I/O) so they can run under deterministic simulation.
* Prefer small, well-named modules; match surrounding style; comment the *why*, not the *what*.

## Dependencies & licensing
* License: MIT. New dependencies must be compatible (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, CDLA-Permissive-2.0: the list in `packaging/about.toml`); after changing dependencies, regenerate `THIRD-PARTY-NOTICES.txt` (command in `CONTRIBUTING.md`). **No GPL/AGPL/LGPL code**. In particular, do not copy from Cryptomator (AGPL/GPL) or abraunegg/onedrive (GPL). Implement from public specs. rclone (MIT) may be consulted, with attribution.
* Crypto: RustCrypto crates only. No hand-rolled primitives. `zeroize` all key material.
* Use `rustls`, not OpenSSL.

## Microsoft APIs
* Use documented Microsoft Graph endpoints. Undocumented/private endpoints are research only: opt-in, behind a feature flag, with Graph as the fallback.
* Honour `Retry-After` and throttling. Never hammer the service.
* Never commit client secrets, tokens or personal account identifiers. The app is a public client (no secret). The client ID is configured locally (see `CONTRIBUTING.md`).

## Documentation duties
* User-facing behaviour change ⇒ update `docs/user-guide/`, written for non-developers.
* Architectural decision ⇒ update `docs/architecture/` (and `private/plan.md` if it changes scope or the roadmap). Architecture pages describe the design as it is now: no dates, phase numbers, version numbers or build history, and nothing personal about the maintainer's PC or data. History belongs in git and the private notes. The same goes for code comments: say what was measured and why it matters, never when or on whose machine.
* Spike or research result ⇒ write it up in `private/spikes/` or `private/research/` with numbers and the date (spike code goes in `private/spikes/code/`); carry the conclusion (not the story) into `docs/architecture/`.
* Status and next steps ⇒ update `private/dev/HANDOFF.md`. Developer guides (building, testing, the app registration) are in `private/dev/`; `CONTRIBUTING.md` keeps what an outside contributor needs.

## Platform notes
* Windows 10 1809+ / Windows 11, x86_64 (ARM64 later). The dev shell may be PowerShell or Git Bash.
* The agent runs **unelevated** in the user session. Anything needing admin is an optional separate service and must be justified in docs.
* The official OneDrive client must not sync the same folder. Code must detect an existing OneDrive sync root and refuse to manage it.
