# TunMan

An SSH tunnel manager for Windows. Keeps a handful of `ssh` forwards up, shows
which processes are using each one and how much is going through it, mounts
remote filesystems, runs rclone sync jobs, and lives in the tray.

It is **standalone**. It was written to feed StreamArchiver's proxy pool with
`socks5h://` endpoints, but nothing here depends on StreamArchiver — that
hand-off is one optional button, off by default.

```
TunMan v0.1.0
┌────────────────────────────────────────────────────────────────────────────┐
│ Tunnels │ Mounts │ Sync │ Traffic │ Log                              ⚙     │
├────────────────────────────────────────────────────────────────────────────┤
│ 2 up · 1 down · 7 connections · ↓ 1.2 MB/s ↑ 84 KB/s                      │
├────────────────────────────────────────────────────────────────────────────┤
│    Name    Geo  Server              Exit IP      Avail  Lat    Cap        │
│ ●  vps-fi  🇫🇮   blu@fi.ex 95.216.1.2  95.216.1.2   99.8%  42ms   61%       │
│ ●  vps-de  🇩🇪   blu@de.ex 116.202.1.2 116.202.1.2  100%   31ms   12%       │
│ ○  db      —    blu@bastion 10.0.0.9   —            —      —      —        │
└────────────────────────────────────────────────────────────────────────────┘
```

## What it does

**Three kinds of tunnel**, matching ssh's own flags:

| Kind | Flag | What you get |
|---|---|---|
| SOCKS | `-D` | A local SOCKS5 proxy. This is the one with a `socks5h://` URL. |
| Local | `-L` | A local port that reaches a host the server can see. |
| Remote | `-R` | A port on the server that reaches back to a host here. |

**It keeps them up.** An unexpected exit is retried with a backoff that doubles
from 5 seconds to a 5-minute ceiling — capped, never given up on, because a
tunnel manager exists to outlast the outage. A tunnel that stayed up for a
minute has its failure streak forgiven, so an hour-old tunnel dropping retries
straight away rather than inheriting a stale backoff.

**"ssh is running" is not "the tunnel works."** A forward that failed to bind,
or a session wedged behind a dead NAT, keeps the process alive while carrying
nothing. So a tunnel is only reported *up* once its port actually accepts a
connection, and a probe goes further: it drives a real SOCKS CONNECT through
the proxy to a host you choose.

**Closing the window hides it.** Tunnels keep running; quit from the tray.

**The tables fit the window they are given.** The tunnel table has sixteen
columns and wants about 1350 pixels to show them all; below that they used to
run off the right-hand edge with no scrollbar, taking the row's own Start,
Stop, Edit and Delete buttons with them. Instead each column now declares how
narrow it is still useful at and how willingly it gives way, and the ones that
matter least are dropped first: totals, then caps, then the exit address, and
so on down to uptime. Four columns are never dropped — the status dot, the
name, the address clients connect to, and the buttons — so a row stays
identifiable and operable at any width, down to the 720-pixel minimum. Nothing
is lost by narrowing: select a row and the detail panel spells out every
dropped column, and each row's name carries a summary in its hover. The Mounts
and Sync tables work the same way.

**Its icon lives inside the exe.** Windows takes the icon for a shortcut, an
Explorer listing and the taskbar button of a program launched from a shortcut
out of the executable's own resources — the icon a running app hands to its
window is a separate thing Explorer never sees, which is why an app can show
the right icon in its title bar and a blank page in the Start Menu. The build
script renders the icon into a multi-size `.ico` (16 through 256, since a size
Windows cannot find it scales from one it can) and embeds it. The drawing
lives in one file that both the build script and the app use, so the two can
never disagree.

**It puts itself in the Start Menu.** On every launch TunMan writes
`%APPDATA%\…\Start Menu\Programs\Blu Software\TunMan.lnk` pointing at the
binary that is actually running, grouped in one folder with everything else
from the same author rather than scattered through Programs. Rewriting it every time is the useful part: TunMan runs from
wherever it was built or unpacked, and a shortcut left pointing at a moved exe
fails silently from the Start Menu while the app works fine launched directly.
The shortcut also names the exe as its icon source explicitly, which is what
makes an already-written shortcut pick up a new icon instead of keeping the one
the shell cached for it.
Turn it off in Settings and the shortcut is removed rather than merely left to
go stale.

**Nothing outlives TunMan.** A clean quit stops every child and its own helpers.
A *crash* or a `taskkill /F` skips every destructor, so children are also placed
in a Windows job object that the kernel tears down when TunMan's handle closes —
however it closes. Without that, killing TunMan left an `rclone mount` running
with its drive letter still claimed, which is exactly the orphan this is meant
to prevent.

## Knowing where a tunnel actually goes

**The country and exit IP are measured through the tunnel, not looked up.** For
a proxy the address that matters is the one the far side *presents*, and it is
not always the box you ssh into — a provider can route egress elsewhere, and a
jump-hosted tunnel comes out somewhere else entirely. So the probe asks
Cloudflare's `/cdn-cgi/trace` **through** the tunnel: no API key, no account,
and because the request leaves from the VPS rather than from home, the lookup
itself reveals nothing new about you. When the exit and the server addresses
differ, the row says so.

That single request answers three questions at once, so it also gives the
**latency** column — a full round trip through the tunnel, including the far
side's own latency, not just the hop to the server. The row shows the last
probe and the average of the last few on hover; one slow probe on a busy link
is noise, a raised average is the tunnel being slow.

**Every SOCKS tunnel is measured once, as soon as it is up.** Those three
columns exist only because something asked, so a tunnel that is never probed
shows three em dashes for its whole life — with nothing on the row to say that
a setting on another tab was the reason. One request when a tunnel comes up is
cheap, and it is the difference between a table that answers "where does this
come out" and one that does not.

What the **health probe** setting buys, then, is *repetition*: re-asking on an
interval, which is how a changed exit or a link that got slower gets noticed.
It is off by default because the answer rarely changes; the interval has a
30-second floor, since a probe is a real request and running it constantly
would be measuring the measurement.

The **server's own address** sits beside its hostname, from a plain local DNS
lookup, so you never have to resolve it yourself.

**Availability** is the share of observed time a tunnel has been up, alongside
its restart and consecutive-failure counts. It is counted only while TunMan is
running and resets when TunMan does — it measures the tunnel, not the month.
For the first two minutes it shows `—` rather than a number: a tunnel that
came up one second after it was started is 20-out-of-21 seconds available,
and 95% in red reads as a fault rather than as arithmetic.

A country can be overridden per tunnel, for one that cannot be probed or whose
provider geolocates somewhere misleading.

## Bandwidth caps

Hourly, weekly and monthly limits in MiB, to keep a box off its provider's bad
side. The windows are deliberately not the same shape:

| Window | Shape | Why |
|---|---|---|
| Hourly | Rolling 60 minutes | A clock hour resets at :00, which would let a burst spend the cap twice either side of the boundary |
| Weekly | Rolling 7 days | Same reasoning |
| Monthly | **Calendar** month | That is how transfer quotas are actually billed |

Usage is kept in hour-sized buckets on disk, because a cap that forgets
everything on restart cannot protect a box from being billed.

At the cap, the default is to **refuse new connections**: transfers already
running finish, the tunnel stays up, and it recovers on its own when the window
rolls over. *Stop the tunnel* and *warn only* are the alternatives; a
cap-stopped tunnel restarts itself once the window moves.

**Caps only work on metered tunnels.** Windows exposes no per-socket byte
counts, so without metering there is nothing to measure against. Rather than
showing a limit that quietly does nothing, TunMan refuses to save it and the
row shows a warning.

## Seeing what uses a tunnel

Two tiers, because Windows gives one away and charges for the other.

**Always on — who is connected.** The system TCP table names the process behind
every socket, so "which programs are on this tunnel right now" costs one call a
second and interferes with nothing. It cannot tell you how many bytes moved:
that is simply not in the table.

**Opt-in — how much, and where to.** Tick *Meter traffic* on a tunnel and ssh
binds a private port while TunMan takes the advertised one. Every connection is
then accepted by TunMan, passed through to ssh, and copied by tasks that count
bytes as they go. For a SOCKS tunnel the copier also reads the client's opening
bytes as they pass, which is how the destination host appears in the table:

```
PID    Process           Conns  Destination        In       Out
24180  streamarchiver.exe  3/14  youtube.com:443   4.2 MB   118 KB
24180  streamarchiver.exe  1/2   i.ytimg.com:443    880 KB     9 KB
31002  firefox.exe         1/1   7tv.io:443          21 KB     3 KB
```

The process behind a connection is looked up once, when the connection is
accepted and its socket is still in the TCP table. That name is then remembered
for the pid, so rows whose connections have all since closed still say who
opened them rather than going blank — a row that shows only a number is a
connection whose owner had already gone by the time it was asked about, not one
TunMan forgot.

The sniffing only ever *reads* — bytes are forwarded verbatim, so a protocol
quirk the parser does not understand costs the destination label and nothing
else. A **remote forward cannot be metered**: its listening socket is on the
server, so there is nothing here to sit in front of. The checkbox is disabled
for those rather than silently doing nothing.

The cost of metering is one loopback hop, and that all traffic for that tunnel
passes through TunMan — so a TunMan crash drops live connections on metered
tunnels. Unmetered tunnels are unaffected by anything TunMan does after starting
them.

## Logging

Everything `ssh` prints is captured and re-emitted as a log line tagged with the
tunnel's name, which is where the reason a tunnel keeps dropping will be:

```
WARN  ssh: connect to host 127.0.0.1 port 2: Connection refused  tunnel=vps-fi
WARN  tunnel exited; retrying  tunnel=vps-fi ran_for=3 fails=1 wait=5
```

The **Log** tab shows the same stream live, filterable by level, by tunnel, and
by text or regex. It holds the most recent 50,000 lines in memory; the files in
`%APPDATA%\TunMan\logs` go back further and are pruned on a schedule you set.

Two details worth knowing:

- The Log tab and the file log sit under **one** filter, so what you can see in
  the app always equals what is on disk.
- Refreshes pause for up to 45 seconds while you have text selected. egui drops
  a selection whenever the model behind it changes, so without this a live table
  cannot be copied from.

If something floods the log, right-click a line → *Mute lines like this*. Muted
patterns are dropped before they are ever stored, so a runaway source stops
evicting everything else. Mutes last for the session only.

## Mounts

`rclone mount` and `sshfs`, kept up the same way tunnels are — but with a
different definition of working. **A live mount process proves nothing:** a
mount can go stale while its process stays perfectly happy. So a mount only
counts as up once its path can actually be listed, and it is re-checked every
few seconds while it runs; when the path stops answering, the mount is torn
down and remade.

The **retry delay is per mount and configurable**, which is the point of it
existing. Some servers treat an instant reconnect as an attack and ban rather
than reconnect, so a fixed delay beats an eager backoff. Zero uses the same
doubling backoff tunnels use (5 s to a 5-minute ceiling), and *give up after* N
failures stops a permanently broken mount retrying forever.

**A mount point outlives the process that made it.** A mount that ends cleanly
releases its drive letter or directory; one that is killed, crashes, or is cut
off by a BSOD does not. The letter stays claimed with nothing behind it, or the
directory is left sitting there — and both rclone and sshfs refuse to mount onto
a path that already exists. Nothing frees it on its own, so the mount retries
forever against a mount point that will never come back, which looks exactly
like a server problem and is not one.

So the mount point is examined and cleared before every attempt, when TunMan
starts, and again right after TunMan kills a mount tool — the case that causes
it. What "cleared" means is deliberately narrow, because the risk here is
removing something that is not a leftover:

- A **drive letter** is only unclaimed when it does not answer *and* the device
  behind it is a WinFsp mount or a network mapping. A letter pointing at a real
  volume is never touched, answering or not: a failing disk must not have its
  letter taken away by a tunnel manager.
- A **directory** is only ever removed with `remove_dir`, which refuses any
  directory that has contents. No path through this code can delete a file.

Every probe is time-boxed, because a dead mount point does not fail when read —
it hangs, for as long as the filesystem driver takes to give up. A supervisor
waiting on one has stopped supervising. Giving up on a read does not stop it,
though: the thread doing it is still stuck in the filesystem. So only one probe
of a given mount point runs at a time, and while one is outstanding the answer
is "not answering" — which is what silence there means. A wedged mount point
therefore costs one stuck thread, not one per check.

rclone mounts pick from the remotes already in your rclone config, so there is
nothing to retype. **rclone's `sftp` backend does the same job as sshfs**, which
matters on Windows: sshfs needs sshfs-win, a separate install, while an sftp
remote needs nothing you do not already have. Both need WinFsp, and TunMan says
plainly when it is missing rather than letting the error be mysterious.

## Sync

rclone jobs, on a schedule or on demand — the DIY cloud half.

**`sync` deletes.** It makes the destination match the source, which means
removing anything at the destination that is not at the source. Point it at the
wrong path once and it is not a failed transfer, it is data gone. So:

- new jobs default to **copy**, which only ever adds;
- destructive modes are labelled as such, in the picker and on the row;
- every job has a **dry run** that reports what it would do and does none of it,
  with the output in a panel underneath;
- a **backup dir** moves anything that would be deleted or replaced aside
  instead of destroying it — the single most useful safety net there is.

Progress comes live from rclone while a run is in flight. *Skip files newer
than* avoids copying something still being written, and a per-job bandwidth
ceiling keeps a sync from swallowing the line.

## Config

One TOML file at `%APPDATA%\TunMan\TunMan.toml`, meant to be hand-editable:

```toml
[settings]
ssh_path = "ssh"
rclone_path = "rclone"
autostart_tunnels = true

[[tunnel]]
name = "vps-fi"
kind = "socks"
user = "blu"
host = "fi.example.org"
port = 1080
meter = true
auto_start = true

[tunnel.caps]
monthly_mib = 500000
action = "block_new"

[[mount]]
name = "backups"
kind = "rclone"
remote = "nas:backups"
target = "R:"
retry_delay_secs = 120

[[job]]
name = "photos"
mode = "copy"
source = "D:/photos"
dest = "offsite:photos"
interval_mins = 60
```

Bandwidth usage lives beside it in `usage.json`, which is machine-written and
not worth hand-editing.

Every field has a default, so a minimal block like the above loads fine. Saves
are atomic — a temp file then a rename — and **a file TunMan could not parse is
never overwritten**: a typo in a hand-edit surfaces as a banner and saving is
refused until you fix it, rather than costing you the file.

**The version being replaced is kept as `TunMan.toml.bak`.** Atomicity only
protects against a *torn* write; it does nothing about a save that is perfectly
well-formed and wrong — another instance overwriting yours, or a definition
deleted by mistake. One generation back undoes either. If the config goes
missing while a backup is beside it, TunMan says so on startup and offers to
restore it.

Set **`TUNMAN_CONFIG`** to point at a different file. Use it for anything
throwaway — a second instance, a test run — so nothing but the real TunMan can
ever write the real config.

## Authentication

Key or agent by default, run under `BatchMode=yes` so ssh fails fast and loudly
instead of blocking forever on a prompt nobody can see — its stdio is piped, so
an interactive password request would hang the tunnel with no visible reason.

Naming a key file also sets `IdentitiesOnly=yes`. Without it, ssh offers every
key in your agent first, and on a host with a few of those you hit
`MaxAuthTries` and get refused with a perfectly good key in hand.

A password can be stored per tunnel for hosts you cannot key. It is passed to
ssh through an askpass helper (TunMan re-invoked with `--askpass`), which means
it lives in that process's environment, and it is stored in `TunMan.toml` as
plain text. It is masked in the log and in the copied command line, but a key is
safer wherever you can use one.

## Using it with StreamArchiver

Copy a tunnel's URL with 📋, or **📤 Export** all of them to a text file, and
paste into StreamArchiver's proxy settings. `socks5h`, not `socks5`: the `h`
makes the *proxy* resolve DNS, so your home resolver never sees the hostnames
being fetched — resolving locally would leak exactly what routing the traffic
away was meant to hide.

Turning on the integration in Settings adds a **➡ Push to StreamArchiver**
button that writes the URLs into its proxy pool directly. It matches on URL,
only ever adds rows and updates labels, and **never** deletes, disables, or
clears health state — a proxy that pool has benched stays benched.

## Building

```sh
cargo build --release
cargo test
cargo clippy
cargo fmt
```

**Quit TunMan before rebuilding.** Windows locks a running executable, so
`cargo build --release` fails with `failed to remove file ... Access is denied
(os error 5)` and the old binary stays where it is. It is easy to read that as
a build that did nothing, and then to wonder why a change is missing from a
program that was never rebuilt.

After the exe is replaced, a Start Menu entry can still show the *previous*
icon: the shell caches shortcut icons, and the cache does not notice that the
file behind one changed. `ie4uinit.exe -show` rebuilds it.

Requires an `ssh` binary; Windows ships one at
`C:\Windows\System32\OpenSSH\ssh.exe`. Mounts and sync jobs need
[rclone](https://rclone.org/), and mounting needs
[WinFsp](https://winfsp.dev/). sshfs mounts additionally need sshfs-win — or
use an rclone remote on the `sftp` backend, which does the same job. `cargo fmt` is safe to run here — this
repo has had a `rustfmt.toml` since its first commit.
