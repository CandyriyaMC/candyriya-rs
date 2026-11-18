---
layout: default
title: Commands
---

# Commands

Candyriya exposes its built-in command set to two sources: players typing
in Minecraft chat (a leading `/`), and the operator typing into the proxy's
console (the terminal the proxy was started from). Both go through the same
command tree, built on a Brigadier-style model: literal nodes (`/candyriya`,
`info`, `servers`, ...), argument nodes (`<player>`, `<server>`), and per-node
requirements. This page is the reference for every built-in command the
proxy ships. There are no other commands today; anything else you type is
either rejected or forwarded to your backend, depending on the source (see
below).

## Console (proxy-side)

The console is the terminal the proxy runs on. When the proxy detects an
interactive terminal on stdin, it hosts a live prompt; when stdin is not a
terminal (a service manager, a pipe, CI), no prompt is shown and logs go to
stdout/stderr as usual.

Type the command after the prompt, without `/` (it also works with one):

```shell
candyriya > /candyriya info
Candyriya v0.1.0
Uptime: 3h 12m
Config: ./candyriya.toml (log level info)
Players online: 0
Connections: 3 active / 41 accepted / 0 rejected
Backends
- lobby (127.0.0.1:25565) ● online ping 4 ms - 0/100 players
candyriya >
```

Output appears on stdout line by line. Colors follow the same rules as the
log output: ANSI SGR colors when the terminal supports them, plain text when
stdout is piped or `NO_COLOR` is set. Output lines never carry the prompt
suffix; the prompt is printed once, before each command is read, not appended
to command output. Errors (an unregistered or mistyped command, a missing
permission) are printed as one red line starting with `command failed:`.

The proxy logs command executions only when `[advanced] log-command-executions`
is enabled (off by default); the command output itself is not traced, so
output never ends up in the log as well.

## Backend (chat-side)

In Minecraft chat, a leading `/` normally marks the message as a command for
the backend. The proxy's rule is:

- a command the proxy has registered is executed by the proxy instead of
  being forwarded;
- anything else - a command the proxy does not know, or an empty tail after
  `/` - is passed to the backend untouched, exactly as if the proxy were
  not there;
- output goes to the invoking player as chat components, not to the console.

The player-facing commands (`/server`, `/goto`, `/glist`, `/send`) behave
the same whether invoked from chat or console, apart from how the output is
rendered. `/candyriya` is the admin tree and is gated (see below).

> **Current build.** The command tree and its wiring are implemented and
> tested, and the console above is live in the composition root. What does
> not exist yet in the running proxy is the chat-side dispatch path and a
> session roster: today the proxy splices play-state traffic without parsing
> it, so commands typed in chat go straight to the backend, and the console
> is the only source that reaches the built-in tree. Chat dispatch, client
> tab-completion announcement and live player lists land with the runtime
> wiring milestones; see [Architecture](architecture) for the build status.
> Until then, console commands run against an empty roster (see
> [Output formats](#output-formats)).

## Permission model

Every console invocation is the same identity: the literal subject
`console`. The console carries a hard-coded `*` allow grant in the command
layer itself, so every command - including the whole `/candyriya` tree -
always parses and runs; nothing is denied for the console.

Players (chat sources) and backends are different subjects. They start with
**no grants at all**, and every permission check falls through to a
default-deny floor. Each subcommand of `/candyriya` carries its own node;
none of them are implied by the root literal, and typing `/candyriya` alone
is not a valid command. The nodes are:

- `candyriya.command.info` - `/candyriya info`
- `candyriya.command.servers` - `/candyriya servers`
- `candyriya.command.switch` - `/candyriya switch <player> <server>`
- `candyriya.command.end` - `/candyriya end`
- `candyriya.command.reload` - `/candyriya reload [config|plugins]`
- `candyriya.command.ps` - `/candyriya ps [--full] [--watch]`
- `candyriya.command.tree` - `/candyriya tree`

Because the check lives on the tree nodes themselves, a source without the
grant cannot even parse the subtree: to an unauthorized player the gated
commands are invisible - the reply is indistinguishable from an unknown
command. The gated commands are deliberately unavailable rather than
refused with an error message.

Grants come from the configured permission provider - for the built-in
config provider, the `[permissions]` table in `candyriya.toml` (see
[Permissions](permissions) for the engine semantics: longest match wins,
deny wins at equal length, default-deny at the floor). A typical admin grant
is `candyriya.command.*`; `*` grants everything, including everything the
proxy will ever add under this namespace.

The player-facing commands carry no nodes at all, by design: `/server`,
`/goto`, `/glist` and `/send` are ungated for everyone, Velocity-style -
they run from the console as well as from chat. `/server` and `/goto`
additionally require the invocation to come from a player (they switch the
invoker, and a console is not a player); `/glist` and `/send` have no such
restriction. Note that backend identities are their own subjects with their
own effective grant maps - the console's `*` does not leak to backends.

## Command reference

Arguments follow the Brigadier model: `<argument>` is required. Server
names are matched exactly first, then case-insensitively; player names are
matched case-insensitively against the online roster. Unknown arguments
do not parse and the command does not run.

### `/candyriya info`

Prints a one-shot summary of the proxy's state: version, uptime, the
configuration in use (name and effective log level), the number of players
online, and the connection counters (active / accepted / rejected). If any
backends are configured, one line per backend is printed with its health
state, the round trip of the last health-check ping, and its player count.
With no backends configured, a single `No backends configured.` line
follows the counters.

Requires `candyriya.command.info`.

### `/candyriya servers`

Prints one line per configured backend, with name, `host:port` address,
health state (`online`, `offline`, `unknown`), ping, and the player count.
With nothing configured, prints `No servers configured.` instead.

Requires `candyriya.command.servers`.

### `/candyriya switch <player> <server>`

Force-moves `<player>` to `<server>`. `<server>` must name a configured
backend; `<player>` must be online at dispatch time. The name is matched
against the live roster, a switch request is handed to the routing layer,
and the invoker is told the post-move truth - the command never claims a
move happened before the proxy confirms it. Re-switching a player to the
server they are already on is a no-op that reports success.

Requires `candyriya.command.switch`.

```shell
candyriya > /candyriya switch Stak lobby
Switched Stak to lobby
```

### `/candyriya end`

Triggers the same graceful shutdown path as Ctrl-C: the proxy stops
accepting connections, drains in-flight work (10-second budget), shuts the
cache and metrics subsystems down and exits. The command only flips the
shutdown flag; the composition root owns the actual shutdown sequence.
Issuing `/candyriya end` twice does not escalate: the second invocation
reports that shutdown is already in progress.

Requires `candyriya.command.end`.

```shell
candyriya > /candyriya end
Shutting down. See you on the other side.
```

### `/candyriya reload [config|plugins]`

Hot-reloads what can be reloaded without a restart, in two phases:

- `config` - re-reads the configuration file the proxy was started with
  (the `--config` path, default `./candyriya.toml`) and swaps it in
  atomically. On success the backend routing registry is rebuilt from the
  new configuration and swapped atomically as well; a failure here leaves
  both the running configuration and the previous routing untouched. The
  MOTD favicon is re-read from disk, locale overrides under `locales/` are
  reloaded, and `forced-hosts` apply to new connections immediately.
- `plugins` - reloads every plugin from `plugins/` and `libraries/`:
  running plugins are staged and swapped in place, newly discovered plugins
  are resolved through the dependency order and registered, and plugins
  whose files vanished are kept running (deleting files is not an
  unload) and reported as kept. Running plugins survive any failure; the
  new configuration's `[plugins]` limits apply to the staged replacements.
- no argument - both phases, config first. `/candyriya reload` and
  `/candyriya reload config` differ only in whether the plugin phase runs;
  `/candyriya reload plugins` runs only the plugin phase.

What the reload does **not** touch: the listener socket (the bind address
and port are process-lifetime), live sessions, and the forwarding secret -
the secret is read once per process and never rotated implicitly. If you
need a different port, a re-keyed forwarding secret or a different session
model, restart the proxy.

On success the invoker sees one line per phase, for example:

```shell
candyriya > /candyriya reload
config reloaded
1 plugins reloaded, 0 failed
```

A failing phase prints a red line with the reason, the other phase still
runs, and the command exits 0 either way - the reload never crashes the
proxy. Exit code is 1 only when both requested phases succeed.

Requires `candyriya.command.reload`. The console's built-in `*` grant
covers it, so the command works from the console out of the box; player and
backend sources need the grant explicitly.

```shell
candyriya > /candyriya reload config
config reloaded
```

### `/candyriya ps [--full] [--watch]`

Prints the supervision tree as a process-style table:

```text
PID PPID NAME STATE RESTARTS UPTIME RX TX LAST EXIT
```

The default view collapses connection units into one `conn × N` row
carrying the count, the byte sums, the worst state present, the restart
sum and the oldest uptime; every other unit gets its own row. `--full`
expands every unit to its own row. Single-letter states: `R` running,
`F` frozen (park gate closed — a cooperative freeze, TCP backpressure
does the rest), `Z` zombie, `t` restarting, `!` failed-terminal. `RX` and
`TX` are the connection byte counters; `UPTIME` is the unit's own run
time. There is no CPU or RSS column and never will be — the tree does
not measure them. The output ends with a summary line computed from the
tree's own counters (`units: N (conn: M, plugins: K) · zombies: A
(stuck: B)`), followed by a one-line state legend.

`--watch` is console-only: it repaints the same table once per second
until any line is read (press Enter) or Ctrl-C is pressed, then prints
one final ordinary frame, so the screen ends with the full table. The
line that ends the watch is swallowed, not executed — anything typed
during the watch (including `/candyriya end`) is dropped, so exit the
watch first, then run the command. On a
terminal each repaint homes the cursor and erases below the previous
frame, after a single clear at watch entry; with `NO_COLOR`/plain
output (stdout piped) the frames are printed newline-separated and
carry no escape bytes. Other sources (players, backends) see the
ordinary one-shot table instead.

```shell
candyriya > /candyriya ps --watch
```

Requires `candyriya.command.ps`.

### `/candyriya tree`

Prints the same supervision tree as an indented box-drawing tree
(`├──` / `└──` guides, `│` continuation stems), one line per unit with
its kind as a caption, plus a `frozen` marker for parked bodies and the
last exit when present. Units whose parent is not in the snapshot are
promoted to top level rather than dropped. There are no columns and no
summary line — this is the pure shape of the tree, useful when the
table's rows are not the question.

Requires `candyriya.command.tree`.

### `/server <server>`

Switches the invoking player to `<server>`. The backend must be configured;
unknown names are rejected with a line listing the known servers. Players
only - other sources (console, backends) are told that only players can
switch themselves.

### `/goto <server>`

An alias of `/server <server>`, kept for muscle memory. Identical in every
way, permission included (there is none).

### `/glist`

Lists the players online across the network: one total line, then one line
per configured backend in `try`-chain order. The backend line color encodes
its position in the chain: first entry green, second gold, the rest gray.
Players who are between servers count towards the network total but have
no per-backend line of their own. On an empty network a `No players online.`
line follows the total.

```shell
candyriya > /glist
players online: 3
  lobby: 2
  minigames: 1
```

### `/send <target> <server>`

Sends `<target>` to `<server>`. `<target>` is an online player's name
(case-insensitive) or the literal `ALL` (case-insensitive) to fan out one
switch request per online player. A named target must be online at dispatch
time; `ALL` with an empty network prints `No players online.` and moves
nobody. `<server>` must be configured either way. The fan-out prints one
line per failed move, plus a summary line with the moved/failed counts.

```shell
candyriya > /send ALL minigames
Sending 3 players to minigames...
Done: 3 moved, 0 failed
```

## Auto-suggestions

Command arguments that name a server or a player are backed by live data
from the directory, not by static lists:

- `<server>` (`/server`, `/goto`, `/send`, `/candyriya switch`) suggests the
  configured backend names, ordered by `try`-chain priority (then by name),
  filtered by the text typed so far.
- `<player>` (`/send`, `/candyriya switch`) suggests the currently online
  player names, fetched fresh on every completion request, filtered the same
  way. `/send` additionally offers `ALL`.
- Suggestions are case-insensitive prefix matches against the canonical
  spelling; the canonical spelling is what the command then validates
  against.

The console itself is a plain line reader with no tab completion - typed
names are validated at dispatch time instead. For players, completions are
announced through the vanilla declare-commands/tab-complete machinery;
`[advanced] announce-proxy-commands` (default `true`) controls whether the
proxy advertises its command tree to clients, and `tab-complete-rate-limit`
bounds how often per player completion requests are accepted. Both are
wired with the chat-side dispatch (see [Backend (chat-side)](#backend-chat-side)).

## Output formats

Console output is rendered from MiniMessage to ANSI: named colors map to
the bright half of the 16-color palette, hex colors to truecolor, bold and
italic to SGR bold/italic. Click and hover decoration is dropped (a console
has no mouse). Lines that fail the MiniMessage gate are printed raw with
terminal control characters stripped - output is never lost to a parse
error, and no config string can inject escape sequences into your terminal.
When stdout is not a terminal or `NO_COLOR` is set, the same lines are
stripped to plain text.

Player-facing output is rendered to chat components on the wire, never as
raw MiniMessage or legacy formatting codes.

Two things are deliberately **not** part of command output:

- **Metrics.** `/candyriya info` and friends print the snapshot a player of
  the operator sees; the numeric counters live in the Prometheus exporter
  (`[metrics] prometheus`, off by default). Command output never embeds
  metric values as a second monitoring channel.
- **Detailed diagnostics.** A failure line shown to a player says what the
  player can act on (unknown server, not online, failed); the opaque reason
  behind a failed switch is logged, not printed. Command execution is
  logged only when `[advanced] log-command-executions` is set.

In the current build there is no session roster in the proxy core yet, so
the online roster is genuinely empty: `/glist` reports 0 players,
`/candyriya info` reports 0 players online and backend player counts are 0,
and switches report the target as not online. These are truthful reports of
an empty roster, not placeholder output.

## Troubleshooting

**"command not registered" / unknown command.** Only the commands on this
page exist. Check the spelling; `/candyriya` requires a subcommand
(`info`, `servers`, `switch`, `end`, `reload`, `ps`, `tree`) - a bare
`/candyriya` does not parse.
In the console you get one red `command failed: ...` line. In chat the
message is forwarded to the backend, which answers with its own
unknown-command response - so an "unknown command" from the backend that
you expected to be a proxy command simply means the proxy did not have it
registered (or, today, that the chat dispatch is not wired yet - see
[Backend (chat-side)](#backend-chat-side)).

**"missing permission" - players who should be admin cannot run `/candyriya`.**
Players start with no grants; the console's `*` is its own. Grant the
relevant node - `candyriya.command.info`, `candyriya.command.servers`,
`candyriya.command.switch`, `candyriya.command.end`, `candyriya.command.reload`,
`candyriya.command.ps`, `candyriya.command.tree`,
or the wildcard
`candyriya.command.*` (or `*`) - in the `[permissions]` table of
`candyriya.toml` and reload. A source without the grant cannot even parse
the subtree, so there is no error message to help you: it behaves exactly
like an unknown command. Check that the provider actually loads the table
(a warning about unknown `[permissions]` keys in the log means the key was
ignored), and see [Permissions](permissions) for how to verify what a
subject effectively holds.

**"target not found" when moving a player.** The target must be an online
player at the moment the command runs; names match case-insensitively
against the online roster. A vanished-mid-flight player is reported by the
proxy after the fact, not pre-checked. If nobody moves and you see
`Player <name> is not online.`, the player is not on the network (or, in
the current build, the roster is not wired yet and everyone looks offline).
An unknown server name is rejected before anything happens, with a line
listing the configured servers. Players between servers count toward
`/glist`'s network total but cannot be targeted by name until they land
somewhere.

**No prompt in the console.** The REPL only starts when stdin is a real
terminal. If the proxy runs under a service manager or as a daemon, commands
cannot be typed; run it attached to a terminal (or take the command surface
you need from the config and logs today).

## Extending

There is no plugin command framework yet. Plugins cannot register commands,
slash or otherwise; the built-in set on this page is complete, and adding
a command today means building from source against `candyriya-command`.
The tree is assembled by a registry that validates literals, rejects
duplicates and attaches per-node gates, so a fork that adds a command gets
the same permission and suggestion machinery for free - but that is a
source-level change, not a configuration one. If your network needs another
command, say so on the issue tracker rather than forking silently.
