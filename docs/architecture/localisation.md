# Localisation

OneUpDrive ships in English. The settings window, the tray and the command line look up what they show in a catalogue for the user's language, so a translation is a file, not a code change. User guide: [The settings window](../user-guide/settings-window.md#language-and-formats).

## The English text is the key
Code says the English (`t!("Nothing found.")` in Rust, `tr("Nothing found.")` in the settings page), as gettext does, rather than an identifier. A catalogue maps that text to its translation. A message without a translation, in any language, shows in English: a string nobody has extracted yet, or one another change adds later, still reads correctly, and merging code that adds strings never conflicts with a catalogue.

## Messages
A subset of ICU MessageFormat, which translation tools (Weblate, Crowdin, Poedit) understand:

* `{name}`: an argument. Numbers are written in the regional format.
* `{n, number}`: the same, said explicitly.
* `{n, plural, =0 {Nothing} one {# file} other {# files}}`: chosen by the language's plural rules (CLDR); `#` is the number; `=N` matches exactly. `other` is required.
* Apostrophes: `'{'` is a brace, `''` an apostrophe; any other apostrophe (as in "can't") is itself.

Whole sentences make better messages than pieces glued together: `tr("Stop syncing {name}?", { name })`, not `"Stop syncing " + name + "?"`. When the English differs by a word (on this PC, in OneDrive), use two messages, since other languages may change more than the word.

`oneup_app::l10n` formats messages in Rust: the plural rules of the languages whose rule isn't English's are in `plural` (French, Portuguese, Spanish, Italian, Catalan, Russian, Ukrainian, Belarusian, Polish, Czech, Slovak, Hebrew, Arabic, Danish, Icelandic, Macedonian, and the languages without plurals). The settings page has the same formatter in JavaScript (`tr` in `settings_ui.html`), with the browser's `Intl.PluralRules` and number formats; `bins/oneup/tests/l10n_strings.rs` runs both on the same messages (where node is installed) and requires the same text.

## What's translated
* **The settings page**: its own text (headings, labels, buttons, options, placeholders, titles, `aria-label`s) is looked up piece by piece when the page loads (`translatePage`: each run of text between tags, whitespace run together); everything the script writes goes through `tr()`. Text from the programs (a problem's title, a sync failure's choices, error messages) comes translated or not from them.
* **The tray**: its menu, tooltip and notifications (`tray.rs`).
* **The command line**: `oneup find`, `oneup search` and `oneup problems` so far. The rest prints English; convert a command by wrapping its `println!` text in `t!`, one message per sentence.
* What stays English on purpose: **Copy details** (it's for a bug report), logs, JSON output, command and option names, and the activity log's timestamps (sortable as they are).

## Language and formats
The display language is the first of Windows' preferred display languages (Settings > Time & language > Language) that has a catalogue, by its whole tag (`pt-BR`) or its language (`pt`); an English one before it means English. `ONEUP_LANG` (`de`, or a list like `de-AT,en`) overrides Windows, for trying a translation. Numbers and dates follow the regional format (Settings > Time & language > Region), which can differ from the display language: `oneup_sys::locale` reads it (`GetUserDefaultLocaleName`, the separators and grouping with `GetLocaleInfoEx`, dates with `GetDateFormatEx` and `GetTimeFormatEx`), and the settings page gets its tag for `Intl`. Tests always get English and English number formats, so what they compare doesn't depend on the PC.

The page gets `{"lang", "locale", "strings"}` from `oneup-settings` in place of `"{{L10N}}"` (`Locale::page_json`, `<` escaped). Nothing is fetched: catalogues are built into the programs.

## Adding a language
1. Write the string list again, so it has every current string: `ONEUP_WRITE_STRINGS=1 cargo test -p oneup --test l10n_strings`. It goes to `bins/oneup/locales/template.json`: every message, English as the key, the translation empty.
2. Copy it to `bins/oneup/locales/<tag>.json` (`de`, `pt-BR`) and translate the values. Leave a value empty to keep the English. Keep each `{placeholder}`'s name; a plural needs the categories your language uses (`one`, `few`, `many`, `other`…) and always `other`.
3. Add it to `CATALOGUES` in `bins/oneup/src/l10n.rs`: `("de", include_str!("../locales/de.json"))`.
4. If your language's plural rule isn't English's (one for 1, other for the rest) and isn't in `l10n::plural`, add it there from CLDR's Language Plural Rules, with a case in `plural_categories_follow_cldr`.
5. `cargo test -p oneup`: `the_built_in_catalogues_are_well_formed` checks every translation parses and uses only the English's placeholders.
6. Try it: set `ONEUP_LANG=<tag>` and open the settings window or run `oneup problems`.

When strings change later, write the template again and compare it with your catalogue: new keys are new strings; keys the template no longer has are strings that changed or went (their translations do nothing, and can go).
