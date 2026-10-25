# Rclone Tray

A native Rust/GTK4 Linux tray application for mounting existing rclone remotes as local folders.
This is an initial MVP, not an offline-sync or conflict-resolution engine.

## Requirements

- Rust 1.92 or newer and GTK4 development libraries (GTK 4.0+).
- `rclone`, FUSE3 (`fusermount3`), and permission to access `/dev/fuse`.
- A graphical Linux session with D-Bus. A StatusNotifierItem tray host is recommended
  (for example KDE's tray, Waybar's tray module, or GNOME's AppIndicator extension).
  Without a tray host the settings window opens; use **Quit** to stop the backend.
- `kill` from the usual Linux process utilities, used only for graceful cancellation
  of a mount that has not become ready.

On Arch Linux: `sudo pacman -S rust gtk4 rclone fuse3 pkgconf`.
On Debian/Ubuntu: install `libgtk-4-dev rclone fuse3 pkg-config build-essential`
and a sufficiently recent Rust toolchain through rustup.

## Run

```sh
rclone config              # configure accounts separately, if needed
cargo build                 # builds the backend and on-demand settings binary
cargo run
# Or start hidden if a supported tray host is available:
cargo run -- --background
```

Click **Add mount**, choose a configured remote, optionally enter a remote subfolder,
and choose an absolute empty local directory. A missing directory is created on mount.
The tray menu and main window both offer mount, open folder, log, and unmount actions.
Re-launching the application opens the existing instance rather than starting duplicate mounts.

Mount cards show aggregate upload/download speeds, cache usage, queued uploads, and errors.
**Queue…** opens Uploads and Downloads tabs with file names, transferred bytes, speeds,
and progress bars when the size is known. Waiting uploads show their size, delay, and retry
attempts. Downloads are on demand: rclone exposes active downloads, not a waiting download
queue. Missing queue data is shown as unavailable, never as an empty queue. Polling runs
off the UI thread, roughly once per second; lists are virtualized for large queues.
The tray tooltip and menu show combined upload/download speeds, including startup recovery.
Each profile's submenu has separate **Upload** and **Download** speed rows so long account
names do not crowd out the numbers. The tray keeps the desktop theme's `drive-harddisk` icon;
whether its title is shown next to the icon depends on the tray host.
Very short transfers can finish between samples, and cached reads need no download.
Older rclone versions without direction metadata show “Transferring” rather than guessing.
“No active transfers” is not a guarantee that every open file has reached the remote.

Startup is not always immediate. rclone can recover and upload retained cached writes before
creating the FUSE mount, especially with large caches or slow cloud metadata requests.
The app reports **Recovering cache…** when startup upload activity is observed, and shows
file progress even before cache statistics are available. Otherwise, after 30 seconds it
reports **Taking longer…** with a per-profile explanation and access to the log. Neither
state marks the folder as mounted or kills/retries the process. When FUSE becomes ready,
the profile automatically changes to **Mounted**. A child process exit becomes **Needs attention**.

The app inherits rclone's normal configuration lookup and environment, including `RCLONE_CONFIG`.
It does not read, copy, or manage cloud credentials. Encrypted rclone configurations need
rclone's supported noninteractive password setup (for example a password command);
the app cannot answer terminal password prompts.

## Notifications

Native desktop notifications are enabled for mount/unmount results, slow startup,
mount failures, blocked unmounts or Quit, cache exhaustion, upload retries, and completed
upload batches. Clicking a notification opens the app while it is running. Notifications
use the desktop's drive icon and follow its notification settings and Do Not Disturb mode.
A notification service is required, such as Plasma/GNOME's built-in service, mako, or dunst.

Routine polling, speed changes, and individual downloads do not produce notifications.
Repeated events replace the previous notification of the same kind for that profile.
Successful cleanup during Quit stays quiet; cleanup failures still alert. Upload completion
requires previously observed pending uploads followed by a confirmed empty queue and cache
status. Missing telemetry is never treated as completion, and an empty queue is not a
guarantee that applications have closed or flushed all open files.

## Storage and safety

- Profiles: `$XDG_CONFIG_HOME/rclone-tray/profiles.toml` (default `~/.config/...`).
- Per-profile VFS cache: `$XDG_CACHE_HOME/rclone-tray/<profile-id>` (default `~/.cache/...`).
- Per-profile logs: `$XDG_STATE_HOME/rclone-tray/<profile-id>.log`
  (default `~/.local/state/...`). Logs are from the latest mount attempt.
- Login startup: `$XDG_CONFIG_HOME/autostart/rclone-tray.desktop`. Enabling this records
  the current executable path; install a release binary in a stable location first.

Mounts run as your user with `--vfs-cache-mode full`, never with `--allow-other` or root.
Cache size is a **target, not a hard quota**: open files and queued uploads can exceed it.
Keep sufficient free disk space. Nonempty and noncanonical/symlink mountpoints are rejected.

File writes can complete locally before cloud upload finishes. Close files before unmounting.
The app checks rclone's upload queue again before unmounting and refuses while uploads
are pending, cached files need retry, or transfer/cache status cannot be checked. This
also applies to cancelling startup: uploads can already be running before the mount exists.
Do not force-stop a recovering process just because the folder is not mounted yet. An open file
can still acquire new writes after the check; this is not a filesystem-wide flush guarantee.
The app uses normal `fusermount3 -u`, not forced/lazy unmount, and does not delete cache data.
Remounting the same unchanged profile lets rclone recover its cached writes. Do not change
a profile's remote or folder while cached writes may be pending, and do not use external
cache cleaners on pending data. Cache retention is not a backup guarantee.

Each owned mount starts rclone's RC API on a dynamically assigned **127.0.0.1-only** port,
with a fresh random password. Credentials stay in process memory and the child's environment,
not profiles, logs, or command-line arguments. HTTP redirects and proxies are disabled.
The app cannot inspect transfer activity for mounts left by another session. If RC is
unavailable indefinitely, inspect the original rclone process and logs before manually
unmounting in a terminal.

The tray/backend (`rclone-tray`) does not load GTK. It owns mounts, telemetry, and
notifications. Settings and queue windows run in a separate, on-demand `rclone-tray-ui`
process, communicating over a user-private Unix socket. Closing the main window exits
that GTK process and frees its memory; mounts and tray status continue running.
Reopening settings reconnects to the same backend without remounting.
Per-mount rclone processes and their VFS caches still use additional memory.

**Quit** waits for pending uploads
and available cache status, normally unmounts each profile owned by this session, waits
for its rclone process to exit, then closes the app. **Cancel quit** stops waiting without
reversing unmounts already in progress. Busy mounts or cleanup failures pause Quit so you
can close open files and retry. Profiles, logs, and cached data are retained; externally
managed mounts are left alone. Unexpected application termination or desktop logout can
leave mounts behind; the next launch marks these as externally managed rather than
silently taking ownership or killing them. Inspect the original process and use
`fusermount3 -u /absolute/mountpoint` when it is safe, then refresh.

## Build and install locally

```sh
cargo build --release
install -Dm755 target/release/rclone-tray ~/.local/bin/rclone-tray
install -Dm755 target/release/rclone-tray-ui ~/.local/bin/rclone-tray-ui
install -Dm644 data/io.github.rclone_tray.App.desktop \
  ~/.local/share/applications/io.github.rclone_tray.App.desktop
```

Keep both binaries in the same directory. Ensure `~/.local/bin` is on your desktop
session's `PATH`. Autostart launches only the lightweight backend.

### Arch Linux package

With `base-devel`, `rust` (or configured `rustup`), `pkgconf`, `desktop-file-utils`,
and the runtime dependencies installed:

```sh
bash packaging/arch/build.sh
# Install only when ready:
sudo pacman -U target/arch/rclone-tray-0.1.0-1-x86_64.pkg.tar.zst
```

The helper snapshots the current local sources, creates a checksummed PKGBUILD in
`target/arch`, builds in release mode, runs headless tests and desktop-entry validation,
and packages both executables, desktop entry, and README. It does not install anything,
request sudo, or touch running mounts. Run it again after changes. The recipe also
allows native aarch64 builds; that architecture has not been tested here.

No project license or upstream repository URL has been declared yet. The package uses
`LicenseRef-Unknown` for local use; choose a license before distributing it or submitting
an AUR package.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The native integration test is opt-in and requires a disposable local remote and working FUSE:

```sh
config=$(mktemp)
printf '[__tray_test]\ntype = local\n' > "$config"
RCLONE_CONFIG="$config" RCLONE_BWLIMIT=1M cargo test real_mount_lifecycle -- --ignored --nocapture
rm "$config"
```

The integration test covers real FUSE reads/writes, live transfer directions and progress,
queued writes, pending-upload protection, RC authentication, profile persistence, busy-mount
quit failure, cancelling Quit, and draining uploads before automatic unmount and exit.
The regular suite also covers startup recovery before VFS registration, slow startup,
unavailable telemetry, stale samples from a previous session, failed child processes,
unsafe cancellation refusal, and tray rate aggregation with long profile names.

The notification delivery test uses a private D-Bus service, with no display or desktop
notification daemon. It checks the notification payload, icon, priority, replacement,
and click action without showing real notifications:

```sh
dbus-run-session -- env -u DISPLAY -u WAYLAND_DISPLAY RCLONE_TRAY_TEST_BUS=1 \
  cargo test native_notification_delivery_and_click -- --ignored --nocapture
```

For GTK Vulkan swapchain/presentation warnings, a one-run renderer diagnostic is
`GSK_RENDERER=gl cargo run`. The app does not change your renderer or system theme settings.
Desktop behavior and appearance need validation in your own session.

Upcoming work: richer recovery. Provider login remains
owned by `rclone config`.
