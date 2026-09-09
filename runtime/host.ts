// Deno host used by ScriptMCP to introspect and invoke user scripts.
// stdout is reserved for a single JSON message. Script console output is
// redirected to stderr so it cannot corrupt the protocol.

import { pathToFileURL } from "node:url";

const encoder = new TextEncoder();

console.log = (...args: unknown[]) => {
  console.error(...args);
};
console.info = (...args: unknown[]) => {
  console.error(...args);
};

function writeJson(value: unknown): void {
  Deno.stdout.writeSync(encoder.encode(JSON.stringify(value) + "\n"));
}

function stem(path: string): string {
  const file = path.replaceAll("\\", "/").split("/").pop() ?? "script";
  return file.replace(/\.[^.]+$/, "") || "script";
}

function asStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.filter((item): item is string => typeof item === "string");
}

function emptySchema(): Record<string, unknown> {
  return { type: "object", properties: {} };
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

type ToolSpec = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  permissions: string[];
  run: (args: unknown) => unknown;
};

function normalize(mod: Record<string, unknown>, scriptPath: string): ToolSpec {
  const def = mod.default;
  if (def && typeof def === "object") {
    const obj = def as Record<string, unknown>;
    const run = obj.run ?? obj.handler;
    if (typeof run === "function") {
      return {
        name: typeof obj.name === "string" && obj.name ? obj.name : stem(scriptPath),
        description: typeof obj.description === "string" ? obj.description : "",
        inputSchema: isObject(obj.inputSchema) ? obj.inputSchema : emptySchema(),
        permissions: asStringArray(obj.permissions ?? mod.permissions),
        run: run as (args: unknown) => unknown,
      };
    }
  }

  const run = typeof def === "function" ? def : (mod.run ?? mod.handler);
  if (typeof run !== "function") {
    throw new Error(
      "script must export a default function, run(), or { name, description, inputSchema, run }",
    );
  }

  return {
    name: typeof mod.name === "string" && mod.name ? mod.name : stem(scriptPath),
    description: typeof mod.description === "string" ? mod.description : "",
    inputSchema: isObject(mod.inputSchema) ? mod.inputSchema : emptySchema(),
    permissions: asStringArray(mod.permissions),
    run: run as (args: unknown) => unknown,
  };
}

async function loadTool(scriptPath: string): Promise<ToolSpec> {
  const href = pathToFileURL(scriptPath).href;
  const mod = (await import(href)) as Record<string, unknown>;
  return normalize(mod, scriptPath);
}

async function readStdin(): Promise<string> {
  return await new Response(Deno.stdin.readable).text();
}

async function main(): Promise<void> {
  const [command, scriptPath] = Deno.args;
  if ((command !== "introspect" && command !== "invoke") || !scriptPath) {
    throw new Error("usage: host.ts <introspect|invoke> <script-path>");
  }

  const tool = await loadTool(scriptPath);

  if (command === "introspect") {
    writeJson({
      name: tool.name,
      description: tool.description,
      inputSchema: tool.inputSchema,
      permissions: tool.permissions,
    });
    return;
  }

  const raw = (await readStdin()).trim();
  const args = raw ? JSON.parse(raw) : {};
  const result = await tool.run(args);
  writeJson({ ok: true, result });
}

try {
  await main();
} catch (error) {
  const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
  writeJson({ ok: false, error: message });
  Deno.exit(1);
}
