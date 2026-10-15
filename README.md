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

solaris is a terminal AI assistant written from scratch in Rust. It is a full-screen TUI chat client — a streaming transcript, markdown rendering, a real editor, overlays, themes — built on a small layered workspace: a transport crate that speaks three wire protocols and nothing else, a tools crate that runs local commands and files and knows nothing about models, a platform crate that knows the providers, an agent crate that wires the three together and is the only door the app needs, and a reusable terminal UI framework that the app is only one consumer of.

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
> - Eight built-in tools — `read`, `write`, `edit`, `ls`, `grep`, `find` and your platform's shell — run by a multi-round tool loop
> - Skills: a `SKILL.md` under any of the usual directories is advertised to the model by name and description, and its instructions load only when a task calls for them — or when you ask for them with `/skill:<name>`
> - A prompt system: identity, mode, working directory, tool guidance, skills, project rules and user/project addenda; `/reload` refreshes filesystem instructions
> - Mid-turn steering: a message typed while a turn runs joins it; `/queue` lines one up for after it
> - Context compaction: when the conversation nears the model's window, older messages are summarized into a structured checkpoint — updated rather than rewritten on later compactions — and a window that turns out to be full is summarized and retried
> - Multi-line editor with word navigation, `/` command autocomplete, and a command palette
> - Two colour themes, cycled at runtime
> - Build and plan modes, with distinct accents and system prompt
> - A terminal companion rolled from your user id, named and persisted by you
> - Welcome box with mascot, recent activity and rotating tips
> - Session statistics — turns, tokens by bucket, cost and context usage
> - Credentials, companion and history persisted under the platform config directory
>
> **Not yet:** MCP, session files on disk, a real device-code sign-in, and an interactive confirmation before a tool runs.

---

# Getting Started

## Requirements

- **Rust 1.85 or newer** — the workspace uses the 2024 edition.
- A terminal that can render 24-bit colour (Windows Terminal, iTerm2, Alacritty, Kitty, …).
- [ripgrep](https://github.com/BurntSushi/ripgrep) and [fd](https://github.com/sharkdp/fd), for the `grep` and `find` tools. Everything else works without them.

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
| `-t`, `--tools <LIST>` | Tools to declare, replacing the mode's default set; a list of `+name`/`-name` entries adjusts it instead | the mode's set |
| `--exclude-tools <LIST>` | Tools to withdraw after everything else has selected them | none |
| `--skill <PATH>` | A skill directory, or a single `SKILL.md`, to load as well as the usual locations; repeat for more than one | none |
| `--confirm-tools` | Ask before a call that changes something (`write`, `edit`, the shell) runs | off |
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
| `/compact` | Summarize the conversation to free up context |
| `/queue` | Run a prompt after the turn in flight |
| `/skills` | List the skills available |
| `/skill:<name> [request]` | Load a skill's instructions |
| `/reload` | Reload skills, project rules and additional system instructions |
| `/clear` | Clear the transcript |
| `/stats` | Show token and cost statistics |
| `/quit` | Exit solaris |

## Modes

**Build** and **plan** are the same conversation with a different accent colour, a different set of declared tools and a different paragraph in the system prompt. `Tab` or `/mode` switches between them, and `--plan` starts you in plan mode. Build mode can run commands and change files; plan mode can only look, and is told so rather than left to discover the missing tools.

## Tools

The model can ask solaris to do things. A turn runs what it asks for, hands back the results, and asks again until it has an answer — all of that is one turn from your side of the screen.

| Tool | What it does | Declared by default in |
| --- | --- | --- |
| `read` | Read a text file, with `offset`/`limit` for large ones | build, plan |
| `write` | Create a file, or replace one | build |
| `edit` | Replace exact text in a file, several places in one call | build |
| `ls` | List a directory | plan |
| `grep` | Search file contents, respecting `.gitignore` | plan |
| `find` | Find paths by glob, respecting `.gitignore` | plan |
| `bash` | Run a shell command and return what it printed | build (not Windows) |
| `powershell` | Run a PowerShell command and return what it printed | build (Windows) |

Plan mode declares only the read-only four, so a plan cannot change anything by accident. `--tools` replaces that set and `--exclude-tools` withdraws from it:

```bash
# read-only for one run, whatever the mode
solaris --tools read,grep,find,ls --print "review this project"

# add one to the mode's default set, and drop another
solaris --tools +grep,-write

# take the shell away entirely
solaris --exclude-tools 'bash'
```

Entries are tool names or patterns where `*` matches any run of characters. A list made only of `+name` and `-name` entries adjusts the mode's default set instead of replacing it, which is how one tool is added without naming the rest. `--print-config` shows what the mode you start in resolves to.

`grep` and `find` shell out to [ripgrep](https://github.com/BurntSushi/ripgrep) and [fd](https://github.com/sharkdp/fd) rather than reimplementing them — they already respect `.gitignore` and walk a tree quickly, and a second implementation would drift. Install both, or leave them out with `--tools`; missing, they report what to install instead of failing the turn.

Every result goes back to the model, failures included: a file that is not there, a command that exited non-zero and a call the model got wrong are all things it can react to, not the end of the turn. Output is cut to keep a turn affordable — the first 2000 lines or 50KB for a read, the last for a command, since that is where a failure prints — and a command whose output was cut has the whole of it written to a file whose path is reported, so the model can read it back in pieces.

Each call appears in the transcript as it happens, with its arguments, how long it took and, when it failed, the first line of what it said.

## Skills

A skill is a directory holding a `SKILL.md`: frontmatter saying what it is and when it applies, then the instructions, usually with scripts and references beside it.

```text
pdf-tools/
├── SKILL.md
├── scripts/
│   └── extract.sh
└── references/
    └── formats.md
```

```markdown
---
name: pdf-tools
description: Extract text and tables from PDF files. Use when reading, converting, or inspecting PDFs.
---

# PDF tools

Read `references/formats.md` before converting a document, and resolve that path
against this skill's directory.
```

solaris tells the model that each skill exists — its name, its description and the file — and nothing else, so the instructions cost no context until a task actually matches. The description is what routes the model to the skill, so it should say what the skill does *and* when it applies. `name` is optional: without one, a `SKILL.md` is named for the directory holding it. The block also takes `disable-model-invocation: true`, which keeps a skill out of the model's list so that only `/skill:<name>` reaches it.

The format is the [Agent Skills](https://agentskills.io/specification) specification, so a skill written for another harness works here unchanged.

Skills are looked for in these directories, in this order, and the first claim on a name keeps it:

| Location | Scope |
| --- | --- |
| `<config dir>/skills` | user |
| `~/.agents/skills` | user |
| `.solaris/skills` in the working directory | project |
| `.agents/skills` in the working directory and its ancestors, up to the repository root | project |

The user's own skills are read first, so a repository cannot shadow one of them by shipping a directory with the same name. Ancestor `.agents/skills` locations are visited nearest first; when there is no repository, discovery stops at the filesystem root. A `.git` file also marks a repository boundary. `--skill <path>` adds a directory, or a single `SKILL.md`, after the default locations. Relative explicit paths resolve from the working directory.

Directories are searched recursively for `SKILL.md`, and a directory holding one is a skill rather than a place to look for more. Real paths prevent duplicate files and symlink cycles, and scanning order is deterministic. Frontmatter is parsed as YAML. Invalid YAML, missing descriptions and name collisions are diagnosed; invalid names warn but still load. Startup diagnostics go to stderr; a bad skill never stops a session.

`/skills` lists what was found, with where each one came from. `/skill:<name> [request]` loads one now, which is how to reach a skill the model did not pick up on its own; whatever follows the name is appended to the instructions as the request. Type `/skill` and the editor offers every skill by name, badged with its source — `[u]` for a user skill, `[p]` for a project one, `[e]` for a path given with `--skill` — and when there are more than fit, the row underneath says which of them is highlighted. `/skill:` on its own is the same as `/skills`.

Explicit calls read the current file and include its absolute path and resource directory. Hidden skills remain in user completion, marked `explicit only`. Skill commands expand consistently in regular input, steering and follow-ups, including `/queue /skill:pdf-tools extract report.pdf`. Unknown names, unreadable files and malformed documents are reported instead of sent as raw commands. Queued messages retain the instructions read when they were queued.

## Project rules and additional prompts

At startup, solaris loads a global rule file from its configuration directory, then project rules from the repository root through the working directory. Without a repository it checks ancestors up to the filesystem root. In each directory it selects the first existing file in this order: `AGENTS.override.md`, `AGENTS.md`, `CLAUDE.md`. Identical real files are included only once, and each instruction block carries its source path.

For extra system instructions, create `<config dir>/APPEND_SYSTEM.md` or `<working directory>/.solaris/APPEND_SYSTEM.md`. Both are appended, user first and project second. The built-in mode and tool guidance remain part of the prompt, which also states the actual working directory.

Run `/reload` after changing rules or addenda, adding/removing skills, or changing skill names and descriptions. It updates the agent's resources and editor completion without clearing conversation history. During a running turn (including queued follow-ups) or compaction, reload waits until the stream is idle. A working-directory load failure preserves the previous resources; individual bad files produce diagnostics. Changes to an explicitly invoked skill's body are read immediately on its next invocation.

Library callers can use `ResourcePaths::load()`, `AgentOptions::with_prompt_context()` and `Agent::replace_resources()` at an idle boundary. Agent-owned queues expose `try_push()` to report skill expansion errors.

## Companion

Every user gets a terminal companion, shown in the welcome box and animated between idle frames. Its *bones* — species, rarity, eyes, hat, shiny — are rolled deterministically from an FNV-1a hash of your user id, so your companion is stable and cannot be hand-edited. Its *soul* — name, personality and hatch time — is written to `companion.json` and yours to set with `/buddy name <name>`.

## Providers

`/connect` opens an inline wizard that takes over the prompt region instead of opening an overlay. It walks through picking a provider and supplying whatever that provider authenticates with: an API key, an OpenAI-compatible base URL plus key, or nothing at all for a runtime on your machine. The catalogue covers Anthropic, OpenCode Go, OpenAI, Google, OpenRouter, a self-hosted New API gateway and a local Ollama/llama.cpp runtime. Most entries list the models they offer; a gateway and a custom endpoint collect their own URL and key instead, and are then asked what they have. Credentials are stored in `auth.json` and masked wherever they are displayed.

Three wire protocols cover the whole catalogue, and the *model* decides which one carries it: the Anthropic Messages API reaches the Claude models, the OpenAI Chat Completions API is the compatibility floor that Google, OpenRouter, New API, a local Ollama/llama.cpp runtime and any custom endpoint speak, and OpenAI's models use the newer Responses API. A provider's own wire is the floor for whatever its table does not list, which is what lets a single OpenCode Go key answer GPT models on the Responses API, Claude models on the Messages API and everything else on the compatibility floor. The key travels the way the wire carrying it asks: `x-api-key` on the Messages API, a bearer token on the OpenAI ones — opencode's Go gateway included, whose `/v1/messages` takes `x-api-key` while its OpenAI-compatible paths take a bearer token. A platform that routes by conversation is also told which one this is, which is how Go attributes a turn to the plan rather than to a pay-as-you-go balance.

### Environment variables

Selecting **OpenAI** in `/connect` now goes directly to ChatGPT browser authorization instead of an API-key field. Press `c` to copy the complete URL, then sign in to OpenAI / ChatGPT and authorize in a browser on the same machine. The callback at `http://localhost:1455/auth/callback` advances the wizard to model selection. Esc cancels; login times out after five minutes. If port 1455 is occupied, close the program using it and retry. Your account must have the required Codex entitlement; this authorization does not grant a general-purpose OpenAI API key.

The flow uses PKCE and a random state. Access, refresh and ID tokens, expiry and account ID are saved locally in `auth.json`. **Treat this as a secret file: do not commit or share it.** Writes use `0600` on Unix and the containing user directory's ACL on Windows. Tokens nearing expiry are refreshed and persisted before model discovery or a new turn. Account tokens only go to the fixed ChatGPT/Codex integration endpoint, ignoring `OPENAI_BASE_URL` and `OPENAI_API_KEY`; they are never forwarded to a custom gateway. The model picker only shows the account's remote Codex catalog, sorted by priority and respecting its visibility flags. There are no bundled fallback models. Loading, an empty catalog and request failures are shown explicitly; press `r` to retry, or reopen `/model` to refresh. The catalog protocol version is independent of Solaris's package version because OpenAI gates catalog entries by Codex client version.

Otherwise, keys found in the environment win over `auth.json`, and a key in the environment is enough on its own: `ANTHROPIC_API_KEY=… solaris` needs no `/connect` at all. Existing OpenAI API-key files and environment configuration remain compatible when no ChatGPT authorization is stored.

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
- **Model names.** Already connected once, solaris asks the provider which models it offers (`GET /models`, the one list endpoint both wires answer) and offers those in `/model`, annotated with the description and price from the bundled catalogue where the model is known. The model is remembered beside the provider it was chosen for — in `auth.json`, where the active provider already lives — and a new session starts on it; when there is no such record, the first model offered is used, because an empty model name is not something a provider will accept. The theme and the mode are remembered as well, in `settings.json`. If the provider will not answer, the bundled catalogue is what the picker shows; if there is nothing there either, `/model` says so rather than sending an empty name and collecting a 400. A name you passed with `--model` or `/model <name>` is kept exactly as written, even when nothing lists it.
- **Errors.** A rejected key, an unknown model, a rate limit or an empty account is reported with the provider's own wording plus what to do about it. The footer always shows the backend that is answering, so a session with no credential says `unconnected` rather than naming a provider it never calls.

---

# Configuration

## Where state lives

solaris resolves a single state directory, in this order:

1. `$SOLARIS_CONFIG_DIR`, when set — handy for keeping a scratch session out of your real config.
2. `%APPDATA%\solaris` on Windows.
3. `$XDG_CONFIG_HOME/solaris`, falling back to `~/.config/solaris`.

`--print-config` prints the paths it actually resolved.

Skills live in `skills/` inside that directory; see [Skills](#skills).

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
