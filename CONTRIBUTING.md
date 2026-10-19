# Contributing to OneUpDrive

Thanks for your interest! OneUpDrive handles people's files, so we value **care over speed**. Every contribution, from a typo fix to a sync-engine change, is welcome.

## Before you start
* Read [Data safety](docs/architecture/data-safety.md). Those rules are non-negotiable. The [architecture overview](docs/architecture/overview.md) explains how the pieces fit.
* For anything bigger than a small fix, open an issue first so we can agree on the approach.
* AI coding agents (and the humans steering them) should also read [`AGENTS.md`](AGENTS.md).

## Development
You need Windows 10 1809+ or Windows 11 (x64), [Rust](https://rustup.rs/) stable (pinned in `rust-toolchain.toml`) and the Visual Studio Build Tools (MSVC and the Windows SDK). `crates/*` are the libraries, `bins/oneup` builds every program (`oneup`, `oneupd`, `oneup-tray`, `oneup-settings`), `packaging/` holds the release script.

Before opening a PR:
```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
The tests run on temp folders and an in-memory OneDrive; nothing touches a real folder or account. A change in what people see happen to their files comes with an end-to-end `everyday_` test in `crates/oneup-exec/tests/sync.rs`; pure decisions (planner, gate, rules) are unit-tested next to the code. CI runs on GitHub's Windows Server runners, one test at a time, and skips a few tests that check how Windows behaves on a desktop PC (listed in `.github/workflows/ci.yml`), so run `cargo test --workspace` locally too.

The repo uses LF line endings (`.gitattributes`); if your editor writes CRLF, run `cargo fmt` and check `git diff`.

### Nightly runs
`.github/workflows/nightly.yml` runs every night (and by hand from the Actions tab) what is too long for CI: the sync simulation (`crates/oneup-sync/tests/sim.rs`) and the mirror simulation (`crates/oneup-remote/tests/mirror.rs`) over millions of seeds, and each fuzz target for half an hour. Each night takes the next window of seeds. A failure opens an issue titled *Nightly run failed* (or comments on the open one) with the failing seed or fuzz input and the command that replays it.

**Replaying a simulation seed.** The issue gives the command; for example:
```powershell
$env:SIM_SEED = "123457"; $env:SIM_DEBUG = "1"
cargo test --release -p oneup-sync --test sim random_histories_converge_without_losing_edits
Remove-Item Env:SIM_SEED, Env:SIM_DEBUG
```
`SIM_SEED` runs that one seed, whatever its number. The simulations also read `SIM_SEEDS` (how many), `SIM_SEED_FROM` (the first) and, in `oneup-sync`, `SIM_STORE=0` (skip the SQLite comparison, which makes a run about 200× slower); in `oneup-remote`, `SIM_DB=memory` keeps each seed's database in memory instead of a file, about ten times faster. To run a range split across processes as the nightly job does: `.github/scripts/sim.ps1 -Package oneup-sync -Test sim -Name <test> -From 1 -Count 100000` (PowerShell 7).

**Fuzzing.** The targets are in `fuzz/` (its own Cargo workspace): `usn_records`, `delta_page`, `push_frames`, `vault_decode` and `rules`, each documented at the top of its file. cargo-fuzz needs the nightly toolchain, and on Windows the AddressSanitizer runtime from the Visual Studio Build Tools (the *C++ AddressSanitizer* component) on `PATH`, so run it from a *Developer PowerShell for VS*:
```powershell
rustup toolchain install nightly
cargo install cargo-fuzz
cargo +nightly fuzz run rules fuzz/corpus/rules fuzz/seeds/rules -- -max_total_time=300
```
`fuzz/seeds/` holds small hand-made inputs (committed); `fuzz/corpus/` is what the fuzzer grows (not committed). To replay a crash from a nightly run, download the `fuzz-<target>` artifact from the run and pass the file instead of the folders: `cargo +nightly fuzz run rules crash-<hash>`. A crash fixed in the library gets a regression test next to the code, not only the fuzz input. The fuzz workspace is a development tool that is never shipped, so it sits outside the licence allow-list: `libfuzzer-sys` bundles LLVM's libFuzzer under the NCSA licence, which is permissive (BSD-style, no GPL terms).

### Signing in from a development build
Release builds have the project's client ID built in; a development build needs your own app registration. In the [Microsoft Entra admin center](https://entra.microsoft.com), register an app for *personal Microsoft accounts only*, with the redirect URI `http://localhost` (platform *Public client/native*) and the delegated Microsoft Graph permissions `Files.ReadWrite`, `Files.ReadWrite.All`, `offline_access` and `User.Read`. No client secret: OneUpDrive is a public client. Then:
```powershell
$env:ONEUP_CLIENT_ID = "<application (client) id>"
cargo run -p oneup -- login --test-account --data-dir "$env:LOCALAPPDATA\OneUpDrive-test"
```
**Try changes only with a separate test Microsoft account and its own data folder**, never your everyday one. `--test-account` marks the folder as a test account's and refuses the drive your default folder mirrors. Keep client IDs out of commits.

## What we look for in a PR
* **Tests.** Behaviour changes come with tests. Changes to anything that can delete, overwrite, move or dehydrate data need tests that show the safety rules still hold, including failure and crash cases.
* **Docs.** User-visible changes update `docs/user-guide/` in plain language. Design changes update `docs/architecture/`.
* **Small, focused changes** with a clear description of *why*.
* **Never use real user data in tests.** Use temp directories and dedicated test accounts only.
* Changes to the safety gate or the destructive-effects layer (`crates/oneup-gate`, `crates/oneup-effects`) need **two reviews**. With one maintainer, that means the maintainer's review plus an **independent model review**: an AI model session that didn't write the change reviews the diff against [Data safety](docs/architecture/data-safety.md), and the PR records the reviewer (model and version), its findings and how each was resolved. The [PR template](.github/pull_request_template.md) has a section for it.

## Licensing
By contributing you agree your work is licensed under the [MIT License](LICENSE). New dependencies must be under a license listed in `packaging/about.toml`; after changing dependencies, regenerate the notices with `cargo about generate -c packaging/about.toml packaging/about.hbs -o THIRD-PARTY-NOTICES.txt` (`cargo install cargo-about --features cli`). CI also runs [`cargo deny check`](https://github.com/EmbarkStudios/cargo-deny) (`cargo install --locked cargo-deny`) with `deny.toml`: licenses, RustSec advisories, duplicate versions (warnings only) and crates.io as the only source. Its license `allow` list must match `accepted` in `packaging/about.toml` exactly; CI fails if they differ, so change both together. An advisory that can't be fixed by updating gets an `ignore` entry in `deny.toml` saying why it doesn't reach OneUpDrive. Each CI run also keeps a CycloneDX SBOM of what ships (the `sbom` artifact). Please don't copy code from GPL/AGPL projects (e.g. Cryptomator, abraunegg/onedrive). Implement from public specifications instead.

## Code of conduct
Be kind and constructive. We follow the [Contributor Covenant](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
