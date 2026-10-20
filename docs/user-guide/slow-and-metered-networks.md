# Slow and metered connections

On a phone hotspot, a paid data plan, a train's Wi-Fi or a slow line at home, you may want OneUpDrive to keep *some* syncing going, cheaply and on your terms. This page covers the settings for that. In the settings window they're on the **Network** page; each also has an `oneup` command.

## Sync every few minutes

By default OneUpDrive syncs within seconds of a change. With a **sync interval** it syncs at most once every so many minutes instead:

* Changes on this PC are still noticed as they happen (that costs no data), and wait for the next sync.
* OneDrive is checked once per interval too, so its changes arrive with the next sync. **Get OneDrive's changes at once** keeps OneDrive's notifications on, so a change there still syncs straight away.
* **Sync now** (in the icon's menu, or `oneup sync-now`) always syncs at once, as does answering a question or resuming.

A busy afternoon of saving files then becomes a few syncs instead of hundreds.

```
oneup settings --sync-interval 15        # sync every 15 minutes
oneup settings --keep-push on            # ...but get OneDrive's changes at once
oneup settings --sync-interval off       # back to within seconds
```

`oneup status` shows the interval and when the next sync runs.

## Settings for each network

OneUpDrive tells networks apart the way Windows does (by the ID Windows gives each network, not by its name, so two Wi-Fi networks called "Home" are still two networks). Each can have its own settings:

* **What syncs** (see below), the **upload and download limits**, the **sync interval**, and a **monthly data budget**.
* A network you haven't set up uses your **default** settings (the limits under **Speed limits**, everything syncs), or the **metered** ones if Windows says the network is metered (a phone hotspot, a data plan you marked as metered in Windows' settings).
* Settings you leave empty for a network come from the default or metered settings.

In the settings window: **Network → This network** changes the network you're on now, and **Metered connections** changes every metered network without settings of its own.

```
oneup network                                              # this network and what syncs on it
oneup network set --this --transfer small --small-mb 20    # on this network, only files under 20 MB
oneup network set --this --sync-interval 30 --upload-kib 200
oneup network set --metered --transfer metadata            # on metered networks, names and folders only
oneup network forget --this                                # this network back to the default or metered settings
```

## What syncs

For metered networks and for each network of its own, choose what syncs there:

| Choice | What happens |
|---|---|
| **Nothing (paused)** | Sync pauses until you're on another network. The default for metered networks. |
| **Everything** | As on any other network. |
| **Names and folders only** | New folders, renames, moves and deletions sync both ways, and with Files On-Demand new files in OneDrive appear here as online-only files. No file contents travel. |
| **Files under the size limit** | Contents of files under the limit (10 MB unless you set another) travel both ways; larger ones wait. |
| **Only files I open or change** | Files you change here upload; nothing downloads in the background. |

Whatever you choose, **opening an online-only file always downloads it**: you asked for that file.

What waits isn't lost or skipped: it syncs as soon as you're on a network that lets it through. `oneup status` says how much waits and why, `oneup network waiting` lists it (largest first), and `oneup explain` says it for one file. If you switch networks in the middle of a sync, OneUpDrive stops starting transfers the new network doesn't allow straight away; the rest waits for the next sync. Files set to **Always keep on this device** download only on a network that lets everything through.

```
oneup network set --metered --transfer small --small-mb 25
oneup network waiting
```

## Data budget

A network's settings (or the metered ones) can have a **monthly data budget** in MB. OneUpDrive counts what it sends and receives on that network, by direction, and shows it in `oneup status` and on the **Network** page (**This network → Data this month**). The count starts again on the first of each month.

* A sync only starts transfers that fit in what's left; the rest waits.
* When the budget is used up, OneUpDrive pauses on that network and tells you (a notification from the OneUpDrive icon). **Allow 500 MB more** (in the icon's menu, on the Network page, or `oneup network allow-more`) adds to this month's budget and syncing goes on.
* Opening an online-only file always downloads it, and counts toward the budget.

```
oneup network set --metered --budget-mb 2000      # 2 GB a month on metered networks
oneup network allow-more --mb 250
oneup network set --metered --budget-mb off
```

## Unreliable connections

OneUpDrive watches how its requests go. When many fail, answers come slowly or files move very slowly, it sends fewer requests at once, sends large files in smaller pieces (so a dropped connection costs less to send again) and waits longer before trying again. `oneup status` then shows **connection: poor**, and the activity log notes it. It goes back to normal once the connection has been good for a while.

A large file whose upload the connection cut off goes on from where it stopped, not from its first byte, as long as OneUpDrive keeps running and the file hasn't changed.
