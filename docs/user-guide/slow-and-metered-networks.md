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
