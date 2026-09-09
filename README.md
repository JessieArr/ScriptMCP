# ScriptMCP

Rust MCP server that turns a directory of Deno scripts into tools. Any MCP client can connect over **Streamable HTTP** on localhost, or spawn the process and talk **stdio**. Each `tools/call` runs the matching script in Deno.

## Requirements

- Rust 1.75+ (edition 2021)
- Deno, either on `PATH` or installed next to the `scriptmcp` binary

## Setup UI

Running `scriptmcp` in a terminal opens a small window that checks for Deno, can install it, scans your scripts folder, starts a localhost HTTP listener, and shows connection details for HTTP and stdio.

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

## Writing a tool

Drop a `.ts` or `.js` file in the scripts directory. Files that start with `_` or `.` are ignored.

Named exports:

```ts
export const name = "hello";
export const description = "Greet someone by name.";
export const inputSchema = {
  type: "object",
  properties: {
    name: { type: "string", description: "Name to greet" },
  },
  required: ["name"],
};
export const permissions = ["net"];

export default function hello({ name }: { name: string }): string {
  return `Hello, ${name}!`;
}
```

Or a default object:

```ts
export default {
  name: "hello",
  description: "Greet someone by name.",
  inputSchema: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
  run({ name }: { name: string }) {
    return `Hello, ${name}!`;
  },
};
```

`name` defaults to the file stem. `run` may be async. A string return becomes MCP text content; any other JSON value is returned as structured content.

### Permissions

Deno runs with `--no-prompt`. By default a script may only read the scripts directory and the embedded host file. Extra rights come from:

- `--allow-all` on the CLI
- `--deno-arg=--allow-env` (repeatable)
- a `permissions` export on the script: `"net"`, `"read"`, `"env"`, `"all"`, or a full flag such as `"--allow-read=/tmp"`

`console.log` from a script is redirected to stderr so it cannot break MCP JSON.

## Layout

- `src/` — Rust MCP frontend (`rmcp` over Streamable HTTP and stdio)
- `runtime/host.ts` — Deno loader that introspects and invokes a script
- `scripts/` — example tools
