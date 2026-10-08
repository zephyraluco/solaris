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

The interface is complete, and a connected provider really is called. Connect a provider — or just export an API key — and every turn streams from the real model over SSE, with retries before the first token and per-turn token and cost accounting.

> [!IMPORTANT]
> **solaris is at v0.1.0, and a connected provider really is called.** Anthropic, OpenAI, Google, OpenRouter, a self-hosted New API gateway and any OpenAI-compatible endpoint — including a local Ollama or llama.cpp runtime — stream over SSE, with retries before the first token and per-turn token and cost accounting.
>
> There is no offline stand-in: with no credential the footer says `unconnected` and a turn reports what to do about it — run `/connect`, or set the provider's key in the environment. The footer always names the backend that is answering.

> [!NOTE]
> **Available today:**
>
> - Real provider clients for the three wire protocols the catalogue covers, chosen from your credentials
> - Full-screen TUI with a retained component tree, flex-like layout stacks and modal/inline overlays
> - Streaming transcript with markdown rendering and collapsible thinking blocks
> - Multi-line editor with word navigation, `/` command autocomplete, and a command palette
> - Two colour themes, cycled at runtime
> - Build and plan modes, with distinct accents and system prompt
> - A terminal companion rolled from your user id, named and persisted by you
> - Welcome box with mascot, recent activity and rotating tips
> - Session statistics — turns, tokens by bucket, cost and context usage
> - Credentials, companion and history persisted under the platform config directory
>
> **Not yet:** tool calling, MCP, session files on disk, and a real device-code sign-in.

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
| `--model <MODEL>` | Model identifier to start with | the connected provider's first |
| `--provider <PROVIDER>` | Send requests to this provider for this run, without changing the saved choice | the active one |
| `--theme <THEME>` | Colour theme to start with (`dark`, `light`) | `dark` |
| `--plan` | Start in plan mode instead of build mode | off |
| `--print-config` | Print the resolved configuration and exit without starting the UI | — |
| `-h`, `--help` | Print help | — |
| `-V`, `--version` | Print version | — |

`--print-config` is the quickest way to see what solaris resolved — model, theme, mode, where credentials live, which backend is answering, the endpoint it would post to, where that credential came from and the companion's state:

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
| `Ctrl+V` | Paste from the clipboard |

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

`/connect` opens an inline wizard that takes over the prompt region instead of opening an overlay. It walks through picking a provider and supplying whatever that provider authenticates with: an API key, an OpenAI-compatible base URL plus key, or nothing at all for a runtime on your machine. The catalogue covers Anthropic, a Claude subscription, OpenAI, Google, OpenRouter, a self-hosted New API gateway and a local Ollama/llama.cpp runtime. Most entries list the models they offer; a gateway and a custom endpoint collect their own URL and key instead, and take the model name from you. Credentials are stored in `auth.json` and masked wherever they are displayed.

Three wire protocols cover the whole catalogue. The Anthropic Messages API carries both Claude entries; the OpenAI Chat Completions API is the compatibility floor that Google, OpenRouter, New API, a local Ollama/llama.cpp runtime and any custom endpoint speak; and OpenAI itself uses the newer Responses API. Which wire a provider speaks is a column in the catalogue, not a guess.

### Environment variables

Keys found in the environment win over `auth.json`, and a key in the environment is enough on its own: `ANTHROPIC_API_KEY=… solaris` needs no `/connect` at all.

| Variable | Provider |
| --- | --- |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL` | Anthropic |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL` | OpenAI |
| `GOOGLE_API_KEY` or `GEMINI_API_KEY` | Google |
| `OPENROUTER_API_KEY`, `OPENROUTER_BASE_URL` | OpenRouter |
| `SOLARIS_LOCAL_BASE_URL` | Local runtime (defaults to `http://localhost:11434/v1`) |

### What to expect

- **Streaming.** Text and thinking arrive as the model produces them; `Ctrl+C` mid-stream cancels the request by dropping the connection.
- **Retries.** A `429`, a `5xx` or a dropped connection is retried up to three times with backoff, but only *before* the first token — after that a retry would duplicate text, so the failure is reported instead.
- **Tokens and cost.** `/stats` breaks a session down by input, output and cache read/write, and prices it. Prices come from a built-in list-price snapshot, so treat them as an estimate rather than a bill; a model the catalogue does not know is reported as free. When a provider reports no usage at all, the numbers are size estimates and `/stats` marks them with `≈`.
- **Prompt caching.** solaris asks for it rather than hoping: an Anthropic `cache_control` breakpoint on the last message, and an OpenAI `prompt_cache_key` when the endpoint is OpenAI's own. That is what makes the cache rows in `/stats` move, and why a long conversation gets cheaper per turn as the unchanged prefix starts coming back as cache reads.
- **Context.** `/stats` reports `session tokens` (everything the session has spent) and `context used` separately. Only the second says how full the window is: it is the last turn's reported usage plus a size estimate for anything submitted since, because summing a session's turns would grow without bound and say nothing about a per-request window.
- **Thinking.** Extended-thinking blocks only appear when the upstream actually streams reasoning (`thinking_delta`, `reasoning_content`, `reasoning`); solaris does not send Anthropic's `thinking` parameter yet, because models that do not support it reject the whole request.
- **Model names.** Name none and the connected provider's first model is used, because an empty model name is not something a provider will accept; the footer shows which one. A name you passed with `--model` or `/model <name>` is kept exactly as written, even when the catalogue does not list it.
- **Errors.** A rejected key, an unknown model or a rate limit is reported with the provider's own wording plus what to do about it. The footer always shows the backend that is answering, so a session with no credential says `unconnected` rather than naming a provider it never calls.

`claude-subscription` is the one gap: signing in over OAuth is not implemented, so the wizard says so and asks you to connect an API key instead. The provider entry, the model list and the credential seam are all in place for when the flow lands.

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
