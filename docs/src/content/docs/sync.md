---
title: Sync across devices
description: Synchronize Aven data with end-to-end encryption, resolve conflicts, and diagnose sync state.
---

Sync keeps the same aven tasks available across laptops, agents, and other devices. Each client writes to its own local SQLite database first, so task capture and updates stay fast and work offline.

Sync is end-to-end encrypted. Devices encrypt tasks, history, and images before
upload; the self-hosted server stores ciphertext and cannot read your data. One
device starts the sync from its database, and each other device joins with an
invitation from a device that already syncs.

The sync server is not a backup. Only your devices hold the decryption keys, so
if every device is lost, the server cannot restore your data. Keep regular
[backups](/backups/).

## Start a server

The sync server is a single `aven server` process that you host yourself on a
machine all your devices can reach. The recommended setup is a private network
such as Tailscale or WireGuard: devices connect to the server's VPN address,
and nothing is exposed to the internet. To reach the server over the public
internet instead, put it behind a TLS reverse proxy and use its HTTPS URL.

Prepare server storage once, giving the URL devices will use to reach it, then
serve it:

```sh
aven server setup --url http://100.100.20.30:3746
aven server --bind 100.100.20.30:3746
```

Storage goes in `~/.local/state/aven/server/sync-server.sqlite`, or in the
directory systemd provides when the service declares `StateDirectory=`. Setup
prints the path it used. To keep it elsewhere, pass the same `--data <path>` to
both commands.

`server setup` prints a setup invitation for the next step. Anyone with it can
claim the server, so use it only on the device whose data should start the
sync. It expires after one hour; run `server setup` again for a new one.

See [`aven server`](/command-reference/#aven-server) for URL rules and bind
options.

### Run the server as a service

Once sync works, run `aven server` under your operating system's service
manager so it starts at boot. On Linux, a systemd user service works; save this
as `~/.config/systemd/user/aven-server.service`, adjusting the binary path and
bind address:

```ini
[Unit]
Description=Aven sync server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/aven server --bind 100.100.20.30:3746
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Then enable it, and enable lingering so it keeps running after you log out:

```sh
systemctl --user daemon-reload
systemctl --user enable --now aven-server
loginctl enable-linger
journalctl --user -u aven-server -f
```

Binding to a VPN address such as a Tailscale IP requires the VPN interface to
be up; if the server fails at boot, systemd retries it every five seconds.

## Set up sync from one device

On the device whose data should start the sync, run setup and paste the setup
invitation:

```sh
aven sync setup
```

Setup shows the server and what this database will publish, then asks for
confirmation. Every other device starts from this data.

In the TUI, open the Sync dialog with `:sync`, `C s`, or a click on the sync
indicator in the header, and choose **Set up sync**.

If setup is interrupted, run the same command again, or choose **Resume
setup**, to continue.

## Add a device

On a device that already syncs, create an invitation and leave the command
running:

```sh
aven sync invite
```

It prints the invitation and shows it as a QR code. In the TUI, choose **Add
device** in the Sync dialog.

:::caution[Keep invitations private]
Anyone with the invitation can read all synced data and manage devices. It
expires after ten minutes. To cancel it early, press Ctrl-C or run
`aven sync invite --cancel`.
:::

On the new device, join from an empty database and paste the invitation:

```sh
aven sync join
```

Confirm the server, and your tasks download, followed by their images. In the
TUI, choose **Join existing sync**. If joining is interrupted, run
`aven sync join` again, or choose **Resume joining**.

A database that already has tasks cannot join, because Aven cannot merge
existing local data into sync. Join with a new database path instead, and use
that path from then on:

```sh
aven --db /path/to/new.sqlite sync join
```

## Automate sync with the daemon

The daemon syncs in the background: after local edits, periodically, and with
retries after failures. Enable automatic sync and install it as a service:

```sh
aven config set sync.enabled true
aven daemon install
```

On macOS this installs a user LaunchAgent; on Linux, a systemd user service.
On Linux, run `loginctl enable-linger` so it keeps running after you log out.
`aven daemon status` shows whether the service is installed and running. See
[`aven daemon`](/command-reference/#aven-daemon) for the other subcommands.

## Sync manually

```sh
aven sync
```

The output lists the changes sent and received and any new conflicts. In the
TUI, press `S`, or choose **Sync now** in the Sync dialog.

### Image attachments during sync

Images sync after their tasks. If sync reports that images are still
transferring, run `aven sync` again or let the daemon finish in the background.

## Check sync status

```sh
aven sync status
```

Status reads local state without contacting the server. It shows the server,
local changes waiting to sync, open conflicts, and pending image transfers. The
Sync dialog in the TUI shows the same overview and whether automatic sync is
on.

If the server refuses this device, status says so. This does not prove the
device was removed; check from another device. Local tasks stay available.

## Manage devices

List the devices in the sync:

```sh
aven sync device list
```

Each device shows its name (the macOS Computer Name or Linux hostname) and a
short ID. Names live in the encrypted data, so the server never sees them.

Remove a device by its ID or a unique prefix of it:

```sh
aven sync device remove 3f9a
```

Removal stops that device from syncing and changes the encryption keys, so it
cannot read future changes. It keeps whatever it already downloaded. Run
removal from another device; a device cannot remove itself. In the TUI, choose
**Manage devices** in the Sync dialog.

To stop syncing this database but keep its data locally, use
[`aven sync reset`](/command-reference/#aven-sync-reset). Reset does not remove
the device from the sync; do that from another device.

A sync allows a limited number of device additions and removals over its
lifetime. When the limit is reached, Aven says so; start a new sync as in
[Recover from device loss](#recover-from-device-loss).

## Resolve conflicts

Conflicts happen when two devices edit the same field of the same task between
syncs. Aven keeps both values and asks you to choose:

```sh
aven conflict list
aven conflict diff APP-7KQ9 description
aven conflict resolve APP-7KQ9 description --use local
```

See [`aven conflict`](/command-reference/#aven-conflict) for exporting both
versions or resolving with a custom value. In the TUI, press `v c` to review
tasks with conflicts.

## Diagnose sync state

`aven doctor` reports whether the database is set up, pending changes,
conflicts, and daemon configuration. Sync errors explain what went wrong and
the next step, with a stable code in square brackets.

## Recover from device loss

If one device is lost or broken, add a replacement as in
[Add a device](#add-a-device), inviting it from a device that still syncs. No
backup is needed. Then remove the lost device from the sync.

If every syncing device is lost, restore a backup to a fresh database path and
start a new sync from it:

```sh
aven --db /path/to/recovered.sqlite backup restore backup.aven-backup.tar.zst --yes
aven server setup --data /path/to/new-sync-server.sqlite --url http://100.100.20.30:3746
aven server --data /path/to/new-sync-server.sqlite --bind 100.100.20.30:3746
aven --db /path/to/recovered.sqlite sync setup
```

Do not reuse the old sync's server storage. Join every other device to the new
sync from an empty database. Changes made after the backup that never synced
are not included; check any old database you can still access before
discarding it.

A syncing database cannot be restored over or imported into, so always restore
to a new path. See [Back up and restore](/backups/) for backup contents.

## Rebuilding sync

Sync stops on a device when it receives a change it can't apply. If Aven on
that device is out of date, update it. If the change itself is damaged, sync
stops at the same place every time; `aven sync` says which case applies. Local
tasks stay safe and editable.

To keep syncing after a damaged change, start a new sync on new server storage
from the device with the best data:

1. Prepare new server storage and serve it, as in
   [Start a server](#start-a-server). Do not reuse the old storage.
2. On the device with the best data, reset sync and set it up again:

   ```sh
   aven sync reset
   aven sync setup
   ```

3. On each other device, check `aven sync status` for local changes that never
   synced, and save anything you need with
   `aven export --output unsynced.json`. Then reset and join the new sync from a
   new database path:

   ```sh
   aven sync reset
   aven --db /path/to/new.sqlite sync join
   ```

## Upgrade from unencrypted sync

Earlier releases synced without end-to-end encryption. Encrypted sync can't
use that server storage, so every device moves to a new sync:

1. Before upgrading, run `aven sync` with the old release on every device.
2. Upgrade Aven everywhere, including the server.
3. Prepare and serve new server storage, as in
   [Start a server](#start-a-server), with a new `--data` path.
4. On the device with the complete data, run `aven sync setup`.
5. Join each other device from a new database path, as in
   [Add a device](#add-a-device).

The old server storage still holds your data unencrypted. Delete it once every
device has joined the new sync.
