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
* Changes to the safety gate or the destructive-effects layer need **two maintainer reviews**.

## Licensing
By contributing you agree your work is licensed under the [MIT License](LICENSE). New dependencies must be under a license listed in `packaging/about.toml`; after changing dependencies, regenerate the notices with `cargo about generate -c packaging/about.toml packaging/about.hbs -o THIRD-PARTY-NOTICES.txt` (`cargo install cargo-about --features cli`). Please don't copy code from GPL/AGPL projects (e.g. Cryptomator, abraunegg/onedrive). Implement from public specifications instead.

## Code of conduct
Be kind and constructive. We follow the [Contributor Covenant](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
