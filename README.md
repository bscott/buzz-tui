# buzz-tui

A terminal client for [Buzz](https://github.com/block/buzz) relays.

Buzz is a self-hostable workspace where humans and agents share the same rooms,
built on Nostr: every message, reaction, and membership change is a signed
event. `buzztui` speaks that protocol directly — NIP-29 groups over WebSocket,
NIP-42 authentication, NIP-17 gift-wrapped direct messages — and renders it at
whatever resolution your terminal can manage.

```
 channels  2             │ # engineering  ·  where the work happens          ● online  localhost:3000
 # engineering        4  │
 # releases              │   today ──────────────────────────────────────────────────────────────────
                         │
                         │  alice  15:37
                         │  morning — the relay migration is merged
                         │   🎉 1
                         │
                         │  bob  15:38
                         │  nice. did the `h` tag enforcement land too?
                         │
                         │  ┌ bob: nice. did the `h` tag enforcement land too?
                         │  alice  15:39
                         │  yes, kind:9 without #h is rejected now
                         │
                         │┌──────────────────────────────────────────────────────────────────────────┐
                         ││ message — / for commands, f1 for keys                                    │
                         │└──────────────────────────────────────────────────────────────────────────┘
 NAVIGATE  i write a message  r reply in thread  s add reaction  ctrl+k jump to a channel  f1 keys
```

## What it does

- **Channels, threads, and direct messages.** NIP-29 groups with NIP-10
  threading, and NIP-17 gift-wrapped DMs that never leave the relay in plaintext.
- **Works offline.** Every event is cached in SQLite, so the timeline is on
  screen before the socket finishes its handshake. Messages composed while
  disconnected are queued and published on reconnect.
- **High-resolution images.** Inline pictures use the terminal's own graphics
  protocol — kitty, sixel, or iTerm2 — and fall back to unicode half-blocks.
  Avatars render in the profile panel the same way.
- **Search.** Local full-text search over everything cached, topped up by the
  relay's NIP-50 index.
- **Reactions, edits, deletions, presence, and typing indicators.**
- **Keybindings that are actually yours.** Every action is named and rebindable;
  the help overlay and the hint bar are generated from the live keymap, so they
  can never document a binding you have changed.

## Install

Requires Rust 1.90 or newer.

```bash
git clone https://github.com/bscott/buzz-tui.git
cd buzz-tui
cargo install --path .
```

## Getting started

```bash
buzztui login                       # creates and stores an identity
buzztui --relay ws://localhost:3000 # connect
```

With no relay flag, `buzztui` reads `relay` from its configuration, falling back
to `BUZZTUI_RELAY` and then `BUZZ_RELAY_URL`.

Other subcommands:

| Command | Purpose |
|---|---|
| `buzztui login [--nsec …] [--force]` | Store an identity, generating one if none is given |
| `buzztui whoami` | Print your public key as npub and hex |
| `buzztui keys` | Print the active binding table, including your overrides |
| `buzztui doctor` | Check the relay URL, identity, database, graphics protocol, and keymap |
| `buzztui paths` | Print where everything lives |

## Files

Everything lives in `~/.config/buzztui`, or `$BUZZTUI_HOME` when set.

| Path | Contents |
|---|---|
| `config.toml` | Relay, interface, and media settings |
| `keys.toml` | Your keybinding overrides — optional |
| `secret.key` | Your identity, written `0600` |
| `buzz.db` | Cached events, channels, profiles, and read state |
| `media/` | Downloaded images |
| `buzztui.log` | Diagnostics; set `BUZZTUI_LOG=buzztui=debug` for more |

Your secret key is your account. Back up `secret.key`; there is no recovery.

## Configuration

`config.toml`, with the defaults shown:

```toml
relay = "ws://localhost:3000"

[ui]
sidebar_width = 26
theme = "buzz"          # buzz | tokyo-night | gruvbox | paper | terminal
clock_24h = true
compact = false
avatars = true          # false skips the download, not just the drawing
backfill = 200          # messages requested when a channel is opened
send_typing = true
send_presence = true

[media]
inline_images = true
max_rows = 16           # tallest an inline image may be
max_bytes = 16777216
protocol = "auto"       # auto | kitty | sixel | iterm2 | halfblocks
cell_size = [10, 20]    # assumed only when the terminal will not report one

# Optional: override individual palette tokens on top of the chosen theme.
[theme]
accent = "#f5c2e7"
```

`theme = "terminal"` uses your terminal's own colours rather than a fixed
palette.

## Keybindings

Three scopes. The composer owns every unmodified printable key, so nothing
reachable while typing can make a character untypable — there is a test that
enforces this.

- **compose** — the composer has the cursor; readline chords apply.
- **navigate** — `esc` from the composer; single keys move around.
- **leader** — `ctrl+b`, then a key. A hint bar lists the continuations.

The essentials:

| Key | Does |
|---|---|
| `esc` / `i` | Leave the composer / return to it |
| `j` `k` | Select the next or previous message |
| `r` `e` `d` `s` `y` | Reply, edit, delete, react, copy |
| `n` `p` `N` `P` | Next or previous channel, or unread channel |
| `g g` `G` `g u` | Oldest, newest, first unread |
| `ctrl+k` | Jump to a channel |
| `ctrl+f` / `/` | Search |
| `f1` / `?` | Every binding, searchable |
| `ctrl+b c` `ctrl+b j` `ctrl+b d` | Create, join, or open a direct message |
| `ctrl+b 1`–`9` | Jump to a channel by position |

Run `buzztui keys` for the full table.

### Rebinding

Write `~/.config/buzztui/keys.toml`. Keys are action names — the third column of
`buzztui keys` and of the help overlay. Setting an action replaces its defaults.

```toml
leader = "ctrl+space"

[compose]
send = "ctrl+enter"
newline = ["enter", "shift+enter"]

[navigate]
select_next = ["ctrl+j", "down"]
reply = "ctrl+r"

[leader_keys]
create_channel = "n"
```

Modifiers are `ctrl`, `alt` (or `option`/`meta`), `shift`, and `super` (or
`cmd`). Named keys include `enter esc tab space backspace del ins home end pgup
pgdn up down left right f1`–`f12`, and punctuation such as `minus comma period
slash question colon`. A binding may be one key or a list. Multi-chord sequences
are written with spaces: `"g g"`. `leader+x` expands to your leader followed by
`x`, and `jump_channel` takes a range: `"leader+1..9"`.

A binding that would make a character untypable in the composer is refused and
reported in a banner rather than silently breaking your keyboard.

## Slash commands

Typed into the composer, for the things that need an argument.

`/join` `/create` `/leave` `/dm` `/invite` `/kick` `/topic` `/relay` `/search`
`/theme` `/mute` `/pin` `/read` `/me` `/whoami` `/reload` `/keys` `/quit`

## Direct messages

DMs are NIP-17 gift wraps: one encrypted rumor is wrapped separately for the
recipient and for you, so the relay learns only who a message is for. Actions
with no private form — reactions, deletions, edits, membership — are refused
inside a DM rather than falling back to a public event that would leak metadata.

## Images

At startup the terminal is asked what it can draw, and `buzztui doctor` prints
the answer along with the cell size it will use:

```
graphics   kitty  (cell 9x18)
```

Images are downloaded once into `media/`, decoded off the render path, and
dropped from memory when they scroll away. Anything the terminal cannot draw
with pixels falls back to unicode half-blocks, which always render.

Two things are worth knowing.

**Some terminals will not report their cell size.** Multiplexers in particular
answer the capability query but refuse the geometry one, and the underlying
library discards a perfectly good protocol when that happens. `buzztui` asks
again on its own and assumes `media.cell_size` — only the number of rows an
image occupies depends on it, not the resolution. If pictures look stretched,
set it to your font's real cell size.

**Some hosts claim graphics support they do not deliver.** Herdr answers the
kitty capability query with `OK` and then drops the image payload, so a client
that believes it reserves rows and draws nothing. `buzztui` therefore stays on
half-blocks inside Herdr rather than leaving a hole in the timeline. Set
`media.protocol = "kitty"` to override that if a future version passes graphics
through.

## Development

```bash
cargo test          # 148 tests, no network
cargo run -- doctor
```

## License

MIT. See [LICENSE](LICENSE).
