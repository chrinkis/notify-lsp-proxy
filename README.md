# notify-lsp-proxy

An LSP proxy that sits between a client (e.g. Helix) and a language server,
adding file watching support on behalf of the client.

Some language servers (e.g. Roslyn for C#) send `client/registerCapability`
requests to register `workspace/didChangeWatchedFiles` watchers. Editors that
don't implement this capability (Helix, as of writing) never receive file
change notifications, causing the server to miss external file changes —
rebuilt assemblies, generated code, restored packages, etc.

This proxy intercepts those registrations, watches the requested glob patterns
itself using the OS file watcher, and injects `workspace/didChangeWatchedFiles`
notifications directly into the language server's stdin stream.

## Installation

```sh
cargo install --path .
```

The binary is installed as `notify-lsp-proxy`.

## Usage

```sh
notify-lsp-proxy -- <language-server-binary> [language-server-args...]
```

The `--` separator is required to prevent argument ambiguity.

## Helix and C#

### omnisharp

In `~/.config/helix/languages.toml`, wrap the language server command with the
proxy:

```toml
[[language]]
name = "c-sharp"
language-servers = ["omni"]

[language-server.omni]
command = "notify-lsp-proxy"
args = [
  "--notify-open-files",
  "--",
  "OmniSharp",
  "--languageserver"
]
```

### roslyn-language-server

- requires pull diagnostics (build from source)

In `~/.config/helix/languages.toml`, wrap the language server command with the
proxy:

```toml
[[language]]
name = "c-sharp"
language-servers = ["roslyn"]

[language-server.roslyn]
command = "notify-lsp-proxy"
args = [
  "--",
  "roslyn-language-server",
  "--stdio",
  "--autoLoadProjects"
]
```

> **Note on pull diagnostics:** Helix uses pull-based diagnostics, meaning it
> only requests fresh diagnostics when the buffer changes. There is no automatic
> way to trigger a pull when external files change. After a `dotnet build` or
> similar, make a small edit in the open buffer (e.g. add and remove a space) to
> prompt Helix to re-pull diagnostics from Roslyn.
