# Networks: profiles, intervals and slow links

How the agent decides how much of the network to use. User-facing: [slow-and-metered-networks.md](../user-guide/slow-and-metered-networks.md).

## Which network

`oneup_sys::network` reads the internet connection's profile through WinRT (`NetworkInformation::GetInternetConnectionProfile`): the network's ID from Windows' Network List Manager (`NetworkAdapter.NetworkItem.NetworkId`, the GUID behind Windows' own network profiles), its name, whether it's metered (`ConnectionCost`: fixed or variable cost, roaming, over its limit) and how far it reaches (`GetNetworkConnectivityLevel`). Without an internet profile, the connected profile that reaches furthest stands in, so a captive portal reads as *constrained* rather than as no network. Profiles are keyed by the ID, never by a name: two networks may share a name, and a typed name would be wrong after a rename.

## Profiles (`oneup_app::network`)

Three layers, each setting only what it changes, resolved by the pure `network::active`:

| Layer | Where | Applies |
|---|---|---|
| default | `[agent]` caps, `[sync]` interval | every network; everything transfers |
| metered | `[metered]` | networks Windows says are metered. Paused while `[agent] pause_on_metered` is on (the default); otherwise `[metered] transfer` |
| own | `[networks."{GUID}"]` | that network, over the default (or metered) layer |

A profile carries what transfers, the size limit for "small files", upload and download caps, the sync interval (and `keep_push`) and a monthly budget. The agent resolves it at every wake-up and hands rounds a `network::Policy` through `sync::Controls`; a profile whose transfer is `paused` becomes an automatic pause (`autopause::Conditions::network`). `cap_hours` still limits when caps apply.

## What transfers

`network::Policy` holds the profile's `Transfer` (`everything`, `metadata`, `small`, `opened`, `paused`) and size limit. Only operations that move file contents are ever held (`network::content`): an upload, and a download unless the folder uses Files On-Demand (then it's a placeholder). Names, folders, moves, deletions and placeholders always go; so does hydration, which is the user opening a file and not a sync operation. `opened` lets uploads through and holds background downloads.

Holding is the same mechanism as for files that keep changing or OneDrive being full: after the gate has reviewed the round, `sync::hold_for_network` takes the held operations out of what runs, and their folders keep their dirty marks, so a round on a network that allows them plans them again. Nothing new reaches the gate or the effects layer, and an operation held now is planned from fresh state later; holding a download while the local file changes ends as an ordinary conflict. The follower (new folders' contents in the same round) holds the same way. Files pinned to stay on the device are hydrated only under `everything`.

What waits is kept in `meta` (`net.waiting`, the 500 largest, with the count and bytes) for `oneup status`, `oneup explain` and `oneup network waiting`, and logged once per reason.

**Mid-round:** `control::LivePause` also holds the policy. `round_check` compares it with the policy the round was planned under; once it differs, a content transfer it doesn't allow stops that side for the rest of the round, as a pause does (downloads, whose size isn't known there, stop under a size limit too). The next round plans what waits under the new policy.

## Data budget

`oneup_app::netusage` keeps each profile's use this month in `meta` (`net.usage.<profile>`: month, bytes up and down, bytes allowed on top), so a restart keeps it and a new month starts at zero. The agent counts with `netusage::Meter` over the Graph client's counters (`Throttle::moved` for content sent, `NetStats::bytes` for every response, file contents included) at every wake-up and every 30 s while rounds run, through its own database connection, and adds the difference to the profile in effect then.

What's left goes to the rounds as `Policy::budget_left`: `hold_for_network` lets transfers through in the round's order while they fit and holds the rest, as for the profile's choice, so background transfers stay within the budget (metadata requests and hydration are counted but never held). When nothing is left the agent pauses itself with that reason; the tray notifies once per month and budget, and offers `allow_data`, which `Control` hands to the agent's next look.

## Sync interval (`oneup_app::pace`)

The `Pacer` is pure: the agent tells it what ended its wait (`Woke`) and when a pass began. With an interval, a journal change no longer ends the wait (the observer still reads the journal, so marks and the Local tree stay current), a push notification ends it only with `keep_push`, and the push channel is closed otherwise. The user's calls and a pass that hit `MAX_ROUNDS` run at once; retries and held uploads wait for the next pass. A simulated hour of steady edits at 15 minutes runs 4 passes (`pace::tests`).
