# tunman

An SSH tunnel manager for Windows. Keeps a handful of `ssh` forwards up, shows
which processes are using each one and how much is going through it, and lives
in the tray.

It is **standalone**. It was written to feed StreamArchiver's proxy pool with
`socks5h://` endpoints, but nothing here depends on StreamArchiver — that
hand-off is one optional button, off by default.

```
tunman v0.1.0
┌──────────────────────────────────────────────────────────────────────────┐
│ Tunnels │ Traffic │ Log                                              ⚙   │
├──────────────────────────────────────────────────────────────────────────┤
│ 2 up · 1 down · 7 connections · ↓ 1.2 MB/s ↑ 84 KB/s                    │
│ ➕ Add  ▶ Start all  ⏹ Stop all │ 📋 Copy all URLs  📤 Export │ 📂 Logs   │
├──────────────────────────────────────────────────────────────────────────┤
│ ● vps-fi  SOCKS  blu@fi.example.org  socks5h://127.0.0.1:1080  4h 12m    │
│ ● vps-de  SOCKS  blu@de.example.org  socks5h://127.0.0.1:1081  4h 12m    │
│ ○ db      Local  blu@bastion         127.0.0.1:5432            —         │
└──────────────────────────────────────────────────────────────────────────┘
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
connection, and the optional health probe goes further: it drives a real SOCKS
CONNECT through the proxy to a host you choose.

**Closing the window hides it.** Tunnels keep running; quit from the tray. On
exit every `ssh` process is killed along with its own helpers — a tunnel manager
that leaves orphans holding ports is worse than one that drops them.

## Seeing what uses a tunnel

Two tiers, because Windows gives one away and charges for the other.

**Always on — who is connected.** The system TCP table names the process behind
every socket, so "which programs are on this tunnel right now" costs one call a
second and interferes with nothing. It cannot tell you how many bytes moved:
that is simply not in the table.

**Opt-in — how much, and where to.** Tick *Meter traffic* on a tunnel and ssh
binds a private port while tunman takes the advertised one. Every connection is
then accepted by tunman, passed through to ssh, and copied by tasks that count
bytes as they go. For a SOCKS tunnel the copier also reads the client's opening
bytes as they pass, which is how the destination host appears in the table:

```
PID    Process           Conns  Destination        In       Out
24180  streamarchiver.exe  3/14  youtube.com:443   4.2 MB   118 KB
24180  streamarchiver.exe  1/2   i.ytimg.com:443    880 KB     9 KB
31002  firefox.exe         1/1   7tv.io:443          21 KB     3 KB
```

The sniffing only ever *reads* — bytes are forwarded verbatim, so a protocol
quirk the parser does not understand costs the destination label and nothing
else. A **remote forward cannot be metered**: its listening socket is on the
server, so there is nothing here to sit in front of. The checkbox is disabled
for those rather than silently doing nothing.

The cost of metering is one loopback hop, and that all traffic for that tunnel
passes through tunman — so a tunman crash drops live connections on metered
tunnels. Unmetered tunnels are unaffected by anything tunman does after starting
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
`%APPDATA%\tunman\logs` go back further and are pruned on a schedule you set.

Two details worth knowing:

- The Log tab and the file log sit under **one** filter, so what you can see in
  the app always equals what is on disk.
- Refreshes pause for up to 45 seconds while you have text selected. egui drops
  a selection whenever the model behind it changes, so without this a live table
  cannot be copied from.

If something floods the log, right-click a line → *Mute lines like this*. Muted
patterns are dropped before they are ever stored, so a runaway source stops
evicting everything else. Mutes last for the session only.

## Config

One TOML file at `%APPDATA%\tunman\tunman.toml`, meant to be hand-editable:

```toml
[settings]
ssh_path = "ssh"
autostart_tunnels = true

[[tunnel]]
name = "vps-fi"
kind = "socks"
user = "blu"
host = "fi.example.org"
port = 1080
meter = true
auto_start = true
```

Every field has a default, so a minimal block like the above loads fine. Saves
are atomic — a temp file then a rename — and **a file tunman could not parse is
never overwritten**: a typo in a hand-edit surfaces as a banner and saving is
refused until you fix it, rather than costing you the file.

## Authentication

Key or agent by default, run under `BatchMode=yes` so ssh fails fast and loudly
instead of blocking forever on a prompt nobody can see — its stdio is piped, so
an interactive password request would hang the tunnel with no visible reason.

Naming a key file also sets `IdentitiesOnly=yes`. Without it, ssh offers every
key in your agent first, and on a host with a few of those you hit
`MaxAuthTries` and get refused with a perfectly good key in hand.

A password can be stored per tunnel for hosts you cannot key. It is passed to
ssh through an askpass helper (tunman re-invoked with `--askpass`), which means
it lives in that process's environment, and it is stored in `tunman.toml` as
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

Requires an `ssh` binary; Windows ships one at
`C:\Windows\System32\OpenSSH\ssh.exe`. `cargo fmt` is safe to run here — this
repo has had a `rustfmt.toml` since its first commit.
