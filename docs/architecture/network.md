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

## Sync interval (`oneup_app::pace`)

The `Pacer` is pure: the agent tells it what ended its wait (`Woke`) and when a pass began. With an interval, a journal change no longer ends the wait (the observer still reads the journal, so marks and the Local tree stay current), a push notification ends it only with `keep_push`, and the push channel is closed otherwise. The user's calls and a pass that hit `MAX_ROUNDS` run at once; retries and held uploads wait for the next pass. A simulated hour of steady edits at 15 minutes runs 4 passes (`pace::tests`).
