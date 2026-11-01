# Portable settings and initial file choices

## Initial reconciliation

Setup compares the scanned and mirrored metadata before opening the effects layer. The observe planner walks both trees, bounded to one folder's children and its traversal stack. Upload and download candidates are recorded in SQLite's indexed `initial_choice` table, keyed by a 128-bit local file ID or cloud item ID. Local-only candidates default to unselected; cloud-only candidates default to selected. Other files retain the ordinary first-sync merge and conflict behavior.

The settings window and `oneup setup-files` read bounded keyset pages and change the same records. Selecting a folder applies to its existing descendants through the indexed parent relationships; selecting a file includes its ancestors. This uses current identities rather than cached path prefixes, so a move cannot select an unrelated former descendant. Answers survive another preview, restart or rescan. Once an item has synced, excluding it uses the ordinary rules interface.

The store's `Trees` view hides unselected items from both child queries and ID reads. Hidden ID reads return `Missing`, never `Deleted`; no answer generates deletion evidence. New items outside held folders use normal sync. A revision change makes the next planning pass reconsider all folders. Files displays held items as left out during setup and can include them again; `oneup explain` identifies the local choice.

Initial inclusion decisions require durable transactions before setup can proceed. The store refuses changing those decisions through a normal transaction. Candidate visibility is separate from the recorded answer: temporarily excluding an item hides it from setup pages without erasing its choice, so removing an exclusion cannot silently include it.

## Portable document


The account's root contains a managed `oneup.settings` document. Its closed schema contains a format identifier, schema version, the OneDrive folder scope, exclusion presets and patterns, sync interval and push preference, and database wait policy, presets and patterns. Unknown fields, unsupported schemas and malformed values are refused. Credentials, executable paths, hooks, local roots, machine resource settings, pinning, pauses and destructive policies are never serialized or imported. The filename at the sync root is reserved from ordinary content reconciliation, so a received document cannot overwrite local configuration through file sync.

Discovery during setup is read-only. Both documents are shown before import or publication. A review code binds the portable local document, the complete local config stamp, cloud item ID and eTag, and account root identity. Actions re-read the inputs and refuse stale codes. Import validates all fields before atomically replacing only the allowed config keys, preserving other sections and local keys within those sections. Folder scope mismatches are refused for existing pairs.

Publication passes exact reviewed bytes through a pure gate token to the effects layer. The effect rechecks the named child and its identity/eTag, creates only with conflict-fail semantics, and replaces only with `If-Match`. The returned size and QuickXorHash must match. Read transfers are capped at 1 MiB and checked against size, hash and eTag before accepting the document.

The application-owned sharing state records durable intent before a publication. Interrupted committed writes are acknowledged by content comparison; recovery does not upload again. The automatic-publication preference is part of that intent and survives recovery. Account identity is checked before background publication. Both local preferences and the cloud document remain available on refusal or conflict.

## Background behavior

Automatic publication is opt-in. Existing sync rounds check a digest of portable settings; unchanged settings cause no additional network requests. Upload pauses, panic and network transfer policy suppress publication. It has no separate polling loop. A changed cloud eTag stops background publication for review instead of overwriting the other PC's settings. Failures appear in Activity and in the shared-settings review. Disable stops publication without deleting the cloud document. Remote settings are applied only through a reviewed import.
