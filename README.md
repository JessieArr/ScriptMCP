# ScriptMCP

Rust MCP server that turns one or more directories of Deno scripts into tools. Any MCP client can connect over **Streamable HTTP** on localhost, or spawn the process and talk **stdio**. Each `tools/call` runs the matching script in Deno.

## Requirements

- Rust 1.75+ (edition 2021)
- Deno, either on `PATH` or installed next to the `scriptmcp` binary

## Setup UI

Running `scriptmcp` in a terminal opens a window that checks for Deno, can install it, manages script folders, toggles which tools are exposed, starts a localhost HTTP listener, and shows connection details for HTTP and stdio. MCP activity appears in a column on the right.

Script folders and per-tool expose settings are saved to `~/.config/scriptmcp/config.json` (or `$XDG_CONFIG_HOME/scriptmcp/config.json`) and reloaded on the next start. Duplicate tool names are listed together and are mutually exclusive—only one can be exposed at a time.

The installer downloads the official Deno zip from the `denoland/deno` GitHub releases, verifies the published SHA-256 checksum, and places `deno` in the same directory as the ScriptMCP executable (for `cargo run`, that is `target/debug/`).

```bash
cargo run                  # setup UI when stdin is a terminal
cargo run -- ui
cargo run -- install-deno  # same install, no window
```

MCP clients that spawn ScriptMCP with piped stdin still get stdio `serve` by default.

## Transports

MCP has more than one way to talk to a server:

| Transport | When to use | How ScriptMCP serves it |
| --- | --- | --- |
| Streamable HTTP | Client connects to a URL | `scriptmcp serve` listens on `http://127.0.0.1:8788/mcp` |
| stdio | Client launches the process | `scriptmcp serve --stdio`, or piped stdin with no subcommand |

There is also a legacy HTTP+SSE transport in the protocol. ScriptMCP does not implement that; HTTP here is Streamable HTTP on `/mcp`.

The server watches every configured scripts folder. Adding, editing, or removing a tool file (or changing folders / expose checkboxes in the UI) reloads the catalog and sends `notifications/tools/list_changed` so clients can refresh schemas. `serve` and `list` also load folders from the saved config when present; `--scripts` seeds the config on first run.

The default HTTP bind is loopback only (`127.0.0.1:8788`). Override with `--bind` or `SCRIPTMCP_BIND`.

## Run

```bash
cargo run -- list --scripts ./scripts
cargo run -- serve --scripts ./scripts              # HTTP on 127.0.0.1:8788
cargo run -- serve --stdio --scripts ./scripts      # stdin/stdout
```

Logs go to stderr so they never mix with stdio MCP traffic.

```bash
scriptmcp serve --scripts /path/to/scripts
scriptmcp serve --bind 127.0.0.1:9000 --scripts /path/to/scripts
scriptmcp serve --stdio --scripts /path/to/scripts
scriptmcp list --scripts /path/to/scripts
scriptmcp --allow-all --timeout 60 serve --scripts ./scripts
```

## Connect a client

HTTP (while `serve` or the UI is listening):

```json
{
  "mcpServers": {
    "scriptmcp": {
      "type": "http",
      "url": "http://127.0.0.1:8788/mcp"
    }
  }
}
```

stdio (the client starts the process):

```json
{
  "mcpServers": {
    "scriptmcp": {
      "type": "stdio",
      "command": "/absolute/path/to/scriptmcp",
      "args": ["serve", "--stdio", "--scripts", "/absolute/path/to/scripts"]
    }
  }
}
```

Exact field names vary by client; the URL, command, and args are what matter.

## Build

```bash
cargo build --release
```

The executable is `target/release/scriptmcp`. After `scriptmcp install-deno` (or the UI button), Deno lives beside that binary and is picked up automatically. Release builds use thin LTO and strip debug symbols.

### Release packages

`tools/package-release.sh` builds a release binary for every desktop target this machine can link, then writes versioned zip files to `dist/`:

```text
dist/scriptmcp-0.1.0-x86_64-linux.zip
dist/scriptmcp-0.1.0-x86_64-windows.zip
```

Each zip contains the executable, `LICENSE.md`, and `README.md`. Linux is native-only (one zip). Windows is a single binary—MSVC when building on Windows, mingw-w64 when cross-compiling. macOS zips are produced on a Mac (or with osxcross). Use `--native-only` to skip cross-compilation.

## Writing a tool

Each MCP tool is a TypeScript (or JavaScript) module that **default-exports a plain object**. Files that start with `_` or `.` are ignored. The script has no dependency on an MCP SDK.

```ts
export default {
  name: "tool_name",

  description:
    "Describe what this tool does and when the LLM should use it.",

  inputSchema: {
    type: "object",
    properties: {
      value: {
        type: "string",
        description: "Description of this argument",
      },
    },
    required: ["value"],
  },

  permissions: {
    read: { allow: [], deny: [] },
    write: { allow: [], deny: [] },
    net: { allow: [], deny: [] },
    env: { allow: [], deny: [] },
    run: { allow: [], deny: [] },
    sys: { allow: [], deny: [] },
  },

  async run(args) {
    return {
      result: args.value,
    };
  },
};
```

### Supported fields

```ts
export default {
  // Required
  name: string,
  description: string,
  run: async (args) => any,

  // Optional
  inputSchema?: JSONSchema,
  outputSchema?: JSONSchema,

  permissions?: {
    read?: string[] | { allow?: string[], deny?: string[] },
    write?: string[] | { allow?: string[], deny?: string[] },
    net?: string[] | { allow?: string[], deny?: string[] },
    env?: string[] | { allow?: string[], deny?: string[] },
    run?: string[] | { allow?: string[], deny?: string[] },
    sys?: string[] | { allow?: string[], deny?: string[] },
  },

  annotations?: {
    readOnlyHint?: boolean,
    destructiveHint?: boolean,
    idempotentHint?: boolean,
    openWorldHint?: boolean,
  },
};
```

| Field | Role |
| --- | --- |
| `name` | MCP tool name. If omitted, defaults to the file stem. |
| `description` | Exposed to the LLM; explain what the tool does and when to use it. |
| `inputSchema` | JSON Schema for arguments passed to `run()`. The host validates `args` against this before invocation. |
| `outputSchema` | Optional JSON Schema for the structured value returned by `run()`. |
| `permissions` | Deno capabilities requested by the script. The host may restrict or reject these according to its policy. |
| `annotations` | Standard MCP tool behavior hints. |
| `run(args)` | Implements the tool. May be `async`. A string return becomes MCP text content; any other JSON value is returned as structured content. |

### Permissions

Deno runs with `--no-prompt`. By default a script may only read its configured script folders and the embedded host file. Extra rights come from:

- `--allow-all` on the CLI
- `--deno-arg=--allow-env` (repeatable)
- a `permissions` object on the script

Example:

```ts
permissions: {
  read: {
    allow: ["~/*"],
    deny: ["~/.ssh/*"],
  },
  write: {
    allow: ["${workspace}/*"],
    deny: ["${workspace}/.secrets/*"],
  },
  net: ["api.github.com"],
  env: ["GITHUB_TOKEN"],
  run: ["git"],
  sys: ["hostname", "osRelease"],
}
```

A bare string array is shorthand for `{ allow: [...] }`. The object form lets you grant broad access and carve out exclusions.

These map to Deno flags:

```text
--allow-read / --deny-read
--allow-write / --deny-write
--allow-net / --deny-net
--allow-env / --deny-env
--allow-run / --deny-run
--allow-sys / --deny-sys
```

`${workspace}` expands to that script's source folder. Use `"*"` in an allow or deny list for the unrestricted form of that flag (for example `net: ["*"]` → `--allow-net`). Empty lists grant or deny nothing for that capability.

`console.log` from a script is redirected to stderr so it cannot break MCP JSON.

## Layout

- `src/` — Rust MCP frontend (`rmcp` over Streamable HTTP and stdio)
- `runtime/host.ts` — Deno loader that introspects and invokes a script
- `runtime/tool.ts` — TypeScript types for the tool module contract
- `scripts/` — example tools
- `~/.config/scriptmcp/config.json` — persisted folders and tool expose flags
