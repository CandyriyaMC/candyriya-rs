---
layout: default
title: Permissions
---

# Permissions

Candyriya ships its own permission engine in the workspace. It is not a
port of an existing plugin and it is not an external crate; the node
grammar, the resolution algorithm and the provider contract are all
implemented here. This page documents the model, the public API, the
built-in config provider and the cases operators most often need. For the
`[permissions]` section of the configuration file, see
[Configuration](configuration); for where the engine sits in the workspace,
see [Architecture](architecture).

## The pieces

Two crates make up the engine:

- **candyriya-permissions** — the engine: validated nodes, contexts,
  allow/deny grants, the node registry, and the resolver. It knows nothing
  about players, config files or databases; those live behind the provider
  seam.
- **candyriya-permissions-config** — the default, config-backed provider:
  it turns the `[permissions]` table of `candyriya.toml` into the flat
  effective grant list the resolver consumes.

The engine is written to be replaced at the provider boundary. A network
that wants database-backed grants (MongoDB, PostgreSQL, an existing
permission database) installs its own provider instead of the config one;
resolution semantics do not change with the provider.

Everything here is ordinary Rust with no `unsafe` code. Resolution is a
pure function apart from a shared, lock-protected default lookup in the
registry.

## The permission model

### Nodes

A node is a dot-separated string such as `candyriya.command.switch`.
Nodes are validated at construction:

- lowercase ASCII only: `a-z`, `0-9`, `.`, `_`, `-`, and `*` in the two
  positions described below;
- at most 128 bytes;
- no empty segments (leading, trailing or double dots);
- the wildcard `*` is only legal as the whole pattern or as a trailing
  `.*` segment (`foo.*`, `foo.bar.*`). Everything else — `f*o`, `foo*`,
  `*.foo`, `foo.*.bar`, `**` — is rejected.

Matching is case-sensitive; nodes are lowercase by construction, so the
question cannot arise on validated input.

Specificity — the number of literal dot-separated segments, with a trailing
`.*` or the bare `*` counting as zero — drives precedence:

| Pattern | Specificity |
|---------|-------------|
| `*` | 0 |
| `candyriya.command.*` | 2 |
| `candyriya.command.switch` | 3 |

A `prefix.*` pattern matches the node with that prefix followed by one or
more further segments. `candyriya.command.*` matches
`candyriya.command.info` and `candyriya.command.info.edit` — never
`candyriya.command` itself. There are no single-segment wildcards and no
substring matching.

### Grants

A grant attaches a decision to a subject or a group. It carries exactly
one of two values: **allow** or **deny**. There is no inherit pseudo-value;
the absence of a matching grant falls back to the registry default, and
ultimately to deny.

A grant may also carry **contexts** (see below) limiting where it applies,
and an optional **source** string for diagnostics. The source has no
resolution semantics — it exists so logs can say where a grant came from.

### Contexts

A context is a name/value pair narrowing *where* a grant applies, for
example `server=lobby`. Names are restricted to `[a-z0-9_-]` (1–32 bytes);
values are printable ASCII, at most 128 bytes. Matching compares both
parts exactly and case-sensitively.

The engine does not know which context names are meaningful. Networks are
encouraged to standardize on `server=` (backend the player is on),
`world=` (world inside that backend) and `time=` (bucket such as `day` /
`night`), because those are the contexts operators already think in. Any
other name works the same way.

A grant applies only if **every** context on the grant is present with the
same value on the subject — a subset match. A grant with no contexts
applies everywhere. A subject with more contexts than the grant mentions
is fine; extra contexts on the subject never widen or narrow a decision.

### Subjects

A subject is who the decision is about: the console, a player, a group, or
anything else (`SubjectKind::Other`). A subject carries a flat list of
group ids it currently holds and the contexts of the current moment.

Group nesting is **not** expanded by the engine. Providers that model
nested membership must flatten the tree before the resolver sees the list
(the config provider flattens one level; anything deeper is the provider's
job). The resolver only ever sees a flat list of grants.

### The registry

The node registry is where nodes are declared with the value the engine
falls back to when no grant matches, plus a human description for tooling.
Lookups are exact-match; the registry does not interpret wildcards (apart
from the bare `*` node, which is itself a legal pattern). Registering a
duplicate node is an error, never a silent overwrite.

Registration is public API (`PermissionRegistry::register`), intended for
the plugin ecosystem to declare the nodes a plugin understands. Today the
built-in commands do not register nodes — the gated `/candyriya`
subcommand nodes are enforced through the command source directly — so in
a stock deployment the registry is empty and an unmatched node resolves to
deny.

## How resolution works

1. **Applicability.** A grant is applicable to node `N` iff `N` is matched
   by the grant's pattern (`*` matches everything; `prefix.*` matches with
   a dot boundary; otherwise exact) **and** every context on the grant is
   present with the same value on the subject.
2. **Specificity ranking.** Applicable grants are ranked by the number of
   literal segments in their pattern. The highest specificity wins. A
   longer pattern always beats a shorter one, regardless of allow/deny and
   of which group granted it.
3. **Equal-length tie.** Among the top-length candidates, an explicit
   deny wins over an allow. Exact-length ties go to deny, period.
4. **No applicable grants.** Fall back to the registered default for `N`;
   if unregistered, deny. There is no way to get an implicit allow.

Two consequences are worth stating out loud:

- Specificity outranks allow/deny. A 3-segment allow beats a 2-segment
  deny, and a 3-segment deny beats a 2-segment allow.
- Exact matches are not artificially preferred over wildcard matches of the
  same literal length — ties at the top length go to deny. For a
  star-free query node (the normal case), two applicable patterns of equal
  specificity are always identical strings, so ties arise from the same
  pattern granted by different sources (for example, the player directly
  and a group); the decision is the same either way — deny wins.

`Resolution` reports the decision and a `BestMatch` naming the grant that
decided it, for debugging and audits. The `BestMatch` carries no semantics
beyond that: do not branch on which grant won, branch on the decision.

### Precedence, in one table

| Situation | Winner |
|-----------|--------|
| `candyriya.command.*` allow (2) vs `candyriya.command.switch` deny (3) | the deny |
| `candyriya.command.switch` allow (3) vs `candyriya.command.*` deny (2) | the allow |
| allow `candyriya.command.switch` and deny `candyriya.command.switch` (both 3) | the deny |
| no applicable grant, node registered with default allow | the registry default |
| no applicable grant, node registered with default deny | deny |
| no applicable grant, node unregistered | deny |

## The built-in config provider

The default provider reads the `[permissions]` section of `candyriya.toml`.
It is intentionally minimal:

```toml
[permissions]
provider = "config"
default = "default"
grants = { mod = ["candyriya.command.*"], admin = ["*"] }
```

Three keys, one semantics:

- **`provider`** — which provider feeds the engine. `"config"` is built in.
- **`default`** — the name of the group handed to players with no explicit
  assignment. The recommended convention is a group named `default`; it
  does not need to exist in `grants` — an absent group simply grants
  nothing.
- **`grants`** — a table of group name → permission-node list. The key
  `default` names the implicit all-subject group; every other key names a
  group.

The semantics, spelled out:

- **Nodes are validated, never trusted.** Every node string is validated
  when the provider is constructed. `ConfigPermissionProvider` parses the
  whole table in one pass and collects **every** invalid node into a
  single error naming the table key, the 0-based index and the validation
  error — never a single first-failure-stops-all surprise. (The
  `candyriya-config` loader itself only structure-checks the table:
  non-empty keys without control characters, lists of non-empty strings;
  the semantics belong to the provider.)
- **The `default` key is the implicit all-subject group.** Its nodes are
  handed to every Console and Player subject, unconditionally. Group
  subjects get their own table entry instead; `Other` subjects get nothing.
- **Group nodes apply to players that hold the group.** A player holds a
  group when the group name appears in the provider's own
  `player_groups` map (keyed by the subject id — the UUID string) or in
  the flat `subject.groups` list the proxy put on the subject. Both
  sources are unioned; the provider's own map wins on duplicates.
- **Allow-only, no contexts.** Plain strings mean allow. This provider has
  no deny syntax and no context support in v1 — a leading `-` would be
  ambiguous with the node grammar, so it does not exist. Networks that
  need deny grants or per-server contexts need a plugin-backed provider
  (see [Registering permissions from plugins](#registering-permissions-from-plugins)).
- **Wildcards flow through verbatim.** `candyriya.command.*` is stored as
  the pattern `candyriya.command.*`; expansion happens at resolution time,
  not at load time.
- **Deterministic output.** Groups iterate in sorted key order; grants keep
  config order within a bucket; the union is stable for identical inputs;
  duplicate grants are emitted once.
- **Console is not "everyone".** Console subjects receive only the
  `default` bucket — never group grants. In the built-in command source,
  the console additionally carries a hard-coded `*` allow grant, so the
  admin command set works out of the box; that convention lives in the
  command layer, not in the provider.

Entries for players are passed to the provider as a separate map. One
sharp edge to know about today: the `[permissions.players]` shape —
uuid-to-groups assignments — is consumed by
`ConfigPermissionProvider::new`, but it is **not** a first-class field of
`candyriya-config` yet. In `candyriya.toml` it belongs to the loose
`[permissions]` table, which the config loader does not model; unknown
keys under `[permissions]` are logged with a warning and ignored. Wiring
player assignments from the file is on the configuration side of the
composition root, not in the `candyriya.toml` schema yet. Until then,
player-to-group assignment is either done by the proxy through the
provider's `subject.groups` list, or by the config provider constructed
from parts.

## Registering permissions from plugins

There are two different things a plugin may write into `plugin.toml` under
a key named `permissions`, and neither is the engine's node registry:

- **The `plugin.toml` `permissions` list is a host-capability whitelist.**
  Entries are opaque strings that must be non-empty and unique; the API
  crate validates them. The loader does not yet interpret them — host
  functions and capability enforcement land with the plugin milestone —
  and they must not be confused with engine permission nodes. Putting
  `candyriya.command.*` there grants nothing; putting `players.read`
  there makes no decision about `candyriya.command.switch` either.
- **Engine nodes are declared, not granted, in plugins.** The registry
  (`PermissionRegistry::register`) accepts a node, a default value, a
  description and a source string, and rejects duplicates. That is the
  seam designed for plugins to declare the nodes they understand, with
  their intended defaults, so tooling and operators can discover them.
  No plugin host function exposes it yet; declaration-by-plugin ships with
  the host API milestone. Until then, a stock deployment has an empty
  registry and default-deny for unmatched nodes.

For granting, a plugin **is not involved**: grants come from the provider.
The architecture supports plugin-backed providers (the `PermissionProvider`
trait in `candyriya-permissions-config` is the narrow contract the proxy
depends on: one method returning the flat effective grant list), so a
database-backed provider can be named as `provider = "…"` in
`candyriya.toml` once it is installed by a plugin. That is the supported
path to per-server (`server=lobby`) or per-world (`world=world_nether`)
contexts, because only a provider can attach contexts to grants — the
config provider cannot.

Two things do **not** exist today, and this page does not claim they do:
there is no YAML permissions file (the built-in provider is TOML), and
there is no plugin-driven node registration live in the current build.

## Caching

Resolution is deliberately uncached today. The resolver scans the flat
effective grant list — which is small (tens of grants, not thousands) —
and the engine has no state of its own. No permission decision is cached
anywhere in the workspace at the time of writing, and none is cached
behind the engine's back.

The `candyriya-cache` crate (cachegirl) is the workspace's single caching
surface and is the intended home for any resolver cache the proxy layer
adds later: bounded, namespaced, with quota accounting and a scheduled
cleanup. Such a cache would belong to the proxy layer, above the engine —
not inside `candyriya-permissions`, which stays a pure function with no
I/O, no clock and no cache of its own. Mojang session data and forwarding
secrets are never cache candidates; public profile data is. Whatever
lands later, resolution must remain correct with the cache bypassed.

## Node IDs and use-case walkthroughs

The engine is agnostic to what a node means; networks define their own
nodes for their own features. The resolution algorithm, however, is
always the same decision: who is asking, about which node, with which
grants in hand. The three scenarios below walk grant tables through it.
Built-in, node-gated commands in the current release are
`candyriya.command.info`, `candyriya.command.servers`,
`candyriya.command.switch`, `candyriya.command.end`,
`candyriya.command.reload`, `candyriya.command.ps` and
`candyriya.command.tree`; the player-facing commands
(`/glist`, `/send`, `/server`, `/goto`) are deliberately ungated and carry no
node. The `candyriya.command.plugin.*` namespace for the upcoming
`/candyriya plugin` subcommands is specified in
[The plugin namespace](#the-plugin-namespace-upcoming) below and is not in
the tree yet. Node IDs prefixed `example.` below are network-defined
illustrations, not built-ins.

### The plugin namespace (upcoming)

Phase 2 of the native-plugins epic adds the `/candyriya plugin` subcommands
(specified in [Commands](commands#candyriya-plugin); tracked in
[issue #310](https://github.com/CandyriyaMC/candyriya-rs/issues/310)). The
subcommands are not in the tree yet, and the nodes below are expected nodes,
not registered ones. When they land, each subcommand carries one node:

| Node | Gates |
|------|-------|
| `candyriya.command.plugin.list` | `/candyriya plugin list` |
| `candyriya.command.plugin.info` | `/candyriya plugin info <plugin>` |
| `candyriya.command.plugin.load` | `/candyriya plugin load <plugin>` |
| `candyriya.command.plugin.enable` | `/candyriya plugin enable <plugin>` |
| `candyriya.command.plugin.disable` | `/candyriya plugin disable <plugin>` |
| `candyriya.command.plugin.unload` | `/candyriya plugin unload <plugin>` |
| `candyriya.command.plugin.restart` | `/candyriya plugin restart <plugin>` |
| `candyriya.command.plugin.inspect` | `/candyriya plugin inspect <plugin>` |
| `candyriya.command.plugin.logs` | `/candyriya plugin logs <plugin>` |
| `candyriya.command.plugin.tree` | `/candyriya plugin tree` |

The namespace rolls up exactly like the rest of the gated tree:

- `candyriya.command.plugin.*` — allows the whole plugin set (a 3-segment
  pattern matching every `candyriya.command.plugin.<name>`).
- `candyriya.command.*` and `*` — cover the namespace as well.
- The console's built-in `*` allows all of them.
- A player or backend with no matching grant is denied every one of them;
  the namespace is default-deny, like the rest of the tree, and nothing is
  implied by the root literal or the `plugin` literal itself.
- The nodes are not registered with a non-deny default; the command layer
  enforces them, exactly as it does for the other `/candyriya` subcommands.

Granting `candyriya.command.plugin.*` to a group covers the whole plugin
surface; granting the individual nodes gates them one by one.

### Scenario 1 — fly locked to the lobby

A network grants flight — node `example.fly` — only on the lobby. The
grant carries the context `server=lobby`, in a group granted via a
plugin-backed provider (contexts cannot come from `candyriya.toml` today):

| Field | Value |
|-------|-------|
| subject | player, contexts `{ server=lobby }` |
| group grants | allow `example.fly` with context `server=lobby` |

Resolution on the lobby: the grant's pattern matches, and the one context
on the grant is present with the same value on the subject — applicable.
Specificity 1, the only candidate, result **allow**.

On `minigames` the subject's contexts are `{ server=minigames }`. The
grant is not applicable (the `server` context value differs), so no grant
candidates remain: the outcome falls back to the registry default for
`example.fly`, and to **deny** if the node is unregistered — which it is
in a default install. The player cannot fly on `minigames` and can fly on
`lobby`, with one grant and no duplication.

### Scenario 2 — new players ("import") can only join the lobby

A network onboards new players into a group `import` and wants them
routed to `lobby` only. The gate node is `example.route.<name>` per
backend (or one node per target). Grants:

| Group | Grants |
|-------|--------|
| `import` | allow `example.route.*`, deny `example.route.pvp` |
| `members` | allow `example.route.*` |

An `import` player on `/server lobby` resolves `example.route.lobby`:
the wildcard allow applies (specificity 2) — **allow**, and the route
proceeds. On `/server pvp` the same player resolves `example.route.pvp`:
a wildcard allow (specificity 2) competes with a deny (specificity 3) —
**deny**, and the command reports the target as unavailable. The
`members` group is unaffected: `example.route.pvp` has no longer-deny for
them, so the wildcard allows. This is both rules working together —
longest specificity wins, and an equal-length explicit deny beats allow.

### Scenario 3 — deny `/glist`

As shipped, `/glist` carries no permission node: any player may run it,
and the engine is never consulted. A network that wants to exclude it
must gate it — with a plugin-backed gate or plugin-provided command
checking a node such as `example.glist`. Once a gate exists, the deny
shape is immediate:

| Group | Grants |
|-------|--------|
| `mods` | allow `candyriya.command.*` |
| `muted` | deny `example.glist` |

A `muted` mod resolving `example.glist` sees: wildcard allow
(`candyriya.command.*`, 2 literal segments) vs exact deny (`example.glist`,
2 literal segments) — an equal-length tie, and the deny wins. Resolving
`candyriya.command.info` sees the wildcard allow only — the deny does not
apply (different literal segments), so the rest of the admin set stays
reachable. Were the deny the same length as the allow — say both groups
held `example.glist`-shaped patterns of equal length — the deny would
still win on the equal-length tie rule. That is the whole point of the
precedence model: an operator can always carve an exception out of a
wildcard, and a deny at equal length is final.

The reverse also holds, for clarity: grant `candyriya.command.switch` to
a group and deny it to another; a subject holding both grants — say they
are in two groups — is denied, because at equal specificity deny beats
allow regardless of source. If you want allow to win in that case, grant a
*longer* allow (`candyriya.command.switch.force`), which by the
longest-wins rule beats the 3-segment deny.

## Inheritance, in one place

- **Groups are flat.** Subjects carry a flat list of group ids; nesting is
  pre-flattened by the provider or it does not exist. Per-group grants
  apply to subjects that hold the group; per-subject grants apply to the
  subject alone.
- **Console** receives only the `default` bucket from the config provider,
  plus its conventional `*` allow in the built-in command source.
- **Group subjects** receive only their own bucket from the config
  provider, never `default`.
- **Other subjects** receive nothing from the config provider.
- **Union order** (provider config first, then subject groups, duplicates
  removed) affects only which grant is reported as `BestMatch`, never the
  decision.

## Related docs

- [Configuration](configuration) — the `[permissions]` section and the full
  `candyriya.toml` reference
- [Architecture](architecture) — where the engine lives in the workspace
- [Installation](installation) — setting up a network, including online
  and offline mode
