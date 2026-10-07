<div align="center">

<h1>SOLARIS</h1>
<h2><em>A terminal AI assistant you can build on</em></h2>

<p>
    <a href="https://github.com/zephyraluco/solaris"><img src="https://img.shields.io/badge/Built_with-Rust-CE4D2B?style=for-the-badge&logo=rust&logoColor=white" alt="Built with Rust"></a>
    <a href="https://github.com/zephyraluco/solaris"><img src="https://img.shields.io/badge/Version-0.1.0-2E8B57?style=for-the-badge" alt="Version 0.1.0"></a>
    <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue?style=for-the-badge" alt="MIT license"></a>
</p>

<p>
    English · <a href="./README.zh-CN.md">简体中文</a>
</p>

</div>

---

solaris is a terminal AI assistant written from scratch in Rust. It is a full-screen TUI chat client — a streaming transcript, markdown rendering, a real editor, overlays, themes — built on a small layered workspace: a pure-logic domain crate, a backend abstraction, and a reusable terminal UI framework that the app is only one consumer of.

The interface is complete and runs entirely offline. solaris ships with a deterministic **mock backend** that streams a canned thinking trace and a markdown reply chunk by chunk, so streaming, scrolling, markdown layout and token accounting can all be exercised without a network connection or an API key.

> [!IMPORTANT]
> **solaris is at v0.1.0 and currently ships only the mock backend.** The `/connect` wizard collects and stores credentials, and lets you pick a provider and a model, but no provider client is implemented yet — every turn is still answered by the mock. Treat the provider catalogue as the shape of the API, not as a working integration.

> [!NOTE]
> **Available today:**
>
> - Full-screen TUI with a retained component tree, flex-like layout stacks and modal/inline overlays
> - Streaming transcript with markdown rendering and collapsible thinking blocks
> - Multi-line editor with word navigation, `/` command autocomplete, and a command palette
> - Two colour themes, cycled at runtime
> - Build and plan modes, with distinct accents and system prompt
> - A terminal companion rolled from your user id, named and persisted by you
> - Welcome box with mascot, recent activity and rotating tips
> - Session statistics — turns, tokens, cost and context usage
> - Credentials, companion and history persisted under the platform config directory

---

# Getting Started

## Requirements

- **Rust 1.85 or newer** — the workspace uses the 2024 edition.
- A terminal that can render 24-bit colour (Windows Terminal, iTerm2, Alacritty, Kitty, …).

## Build from source

```bash
git clone https://github.com/zephyraluco/solaris.git
cd solaris

# debug build
cargo build

# optimised build — the binary lands at target/release/solaris
cargo build --release
```

## Run

```bash
# from the repository
cargo run --release

# or run the built binary directly
./target/release/solaris
```

## Command-line options

| Option | Description | Default |
| --- | --- | --- |
| `--model <MODEL>` | Model identifier to start with | `solaris-mock-1` |
| `--theme <THEME>` | Colour theme to start with (`dark`, `light`) | `dark` |
| `--plan` | Start in plan mode instead of build mode | off |
| `--chunk-delay-ms <MS>` | Per-chunk streaming delay for the mock backend, in milliseconds | `24` |
| `--print-config` | Print the resolved configuration and exit without starting the UI | — |
| `-h`, `--help` | Print help | — |
| `-V`, `--version` | Print version | — |

`--print-config` is the quickest way to see what solaris resolved — model, theme, mode, where credentials live, the active provider and the companion's state:

```bash
solaris --print-config
```

---

# Using solaris

The prompt is the only text field. Type a message and press `Enter`, or type `/` to bring up the command list — `Tab` completes the highlighted command.

## Keyboard

| Key | Action |
| --- | --- |
| `Enter` | Send the prompt — or run the command under `/` |
| `Alt+Enter` | Insert a newline |
| `Tab` | Toggle build / plan mode — or fill in the `/` command |
| `Ctrl+K` | Command palette |
| `Ctrl+T` | Cycle the theme |
| `Ctrl+O` | Toggle thinking blocks |
| `Ctrl+L` | Clear the transcript |
| `PageUp` / `PageDown` | Scroll the transcript (the mouse wheel works too) |
| `?` | This help, when the prompt is empty |
| `Esc` | Cancel whatever is open |
| `Ctrl+C` | Copy the selection, stop the running turn — press twice to quit |
| `Ctrl+D` | Quit, when the prompt is empty |

## Slash commands

| Command | Description |
| --- | --- |
| `/help` | Show keyboard shortcuts and commands |
| `/connect` | Connect a model provider |
| `/theme [dark\|light]` | Switch the colour theme |
| `/buddy [name <name>]` | Show your companion |
| `/model [name]` | Switch the active model |
| `/mode` | Toggle build / plan mode |
| `/clear` | Clear the transcript |
| `/stats` | Show token and cost statistics |
| `/quit` | Exit solaris |

## Modes

**Build** and **plan** are the same conversation with a different accent colour and a different line in the system prompt. `Tab` or `/mode` switches between them, and `--plan` starts you in plan mode.

## Companion

Every user gets a terminal companion, shown in the welcome box and animated between idle frames. Its *bones* — species, rarity, eyes, hat, shiny — are rolled deterministically from an FNV-1a hash of your user id, so your companion is stable and cannot be hand-edited. Its *soul* — name, personality and hatch time — is written to `companion.json` and yours to set with `/buddy name <name>`.

## Providers

`/connect` opens an inline wizard that takes over the prompt region instead of opening an overlay. It walks through picking a provider and supplying whatever that provider authenticates with: an API key, an OpenAI-compatible base URL plus key, a device-code sign-in, or nothing at all for a runtime on your machine. The catalogue covers Anthropic, a Claude subscription, OpenAI, Google, Groq, OpenRouter and a local Ollama/llama.cpp runtime, together with the models each of them offers.

Credentials are stored in `auth.json` and masked wherever they are displayed. As noted above, no provider client is implemented yet, so connecting a provider does not change where answers come from.

---

# Configuration

## Where state lives

solaris resolves a single state directory, in this order:

1. `$SOLARIS_CONFIG_DIR`, when set — handy for keeping a scratch session out of your real config.
2. `%APPDATA%\solaris` on Windows.
3. `$XDG_CONFIG_HOME/solaris`, falling back to `~/.config/solaris`.

`--print-config` prints the paths it actually resolved.

## Files

| File | Contents |
| --- | --- |
| `auth.json` | Credentials written by `/connect`, keyed by provider id, plus the active provider and model. |
| `companion.json` | The companion's soul: name, personality and hatch time. |
| `recent.json` | Recent activity, shown in the welcome box and used to rotate tips. |

All three are plain JSON. A missing or unreadable file is treated as empty, so deleting one resets that piece of state — it never stops the app from starting.

---

# Acknowledgements

solaris is a Rust port of ideas from two projects, and the source says so where it matters:

- [**claurst**](https://github.com/Kuberwastaken/claurst) — the terminal companion, the two-column welcome box, the inline `/connect` flow and the shape of the provider catalogue all follow its design.
- **`@earendil-works/pi-tui`** — the retained-component-tree TUI design, with application-owned focus and overlays and flex-like layout stacks, follows that library's approach; each `solaris-tui` module ported from it says so in its header.
