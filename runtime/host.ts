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

type CapabilityPermission = {
  allow: string[];
  deny: string[];
};

type Permissions = {
  read: CapabilityPermission;
  write: CapabilityPermission;
  net: CapabilityPermission;
  env: CapabilityPermission;
  run: CapabilityPermission;
  sys: CapabilityPermission;
};

type Annotations = {
  readOnlyHint?: boolean;
  destructiveHint?: boolean;
  idempotentHint?: boolean;
  openWorldHint?: boolean;
};

type ToolSpec = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  permissions: Permissions;
  annotations?: Annotations;
  run: (args: unknown) => unknown;
};

type FormattedError = {
  name: string;
  message: string;
  stack?: string;
};

const CAPABILITY_KEYS = ["read", "write", "net", "env", "run", "sys"] as const;

function emptyCapability(): CapabilityPermission {
  return { allow: [], deny: [] };
}

function emptyPermissions(): Permissions {
  return {
    read: emptyCapability(),
    write: emptyCapability(),
    net: emptyCapability(),
    env: emptyCapability(),
    run: emptyCapability(),
    sys: emptyCapability(),
  };
}

function normalizeCapability(value: unknown): CapabilityPermission {
  if (Array.isArray(value)) {
    return { allow: asStringArray(value), deny: [] };
  }
  if (!isObject(value)) {
    return emptyCapability();
  }
  return {
    allow: asStringArray(value.allow),
    deny: asStringArray(value.deny),
  };
}

function applyFlagList(
  out: Permissions,
  mode: "allow" | "deny",
  key: string,
  list: string[],
): void {
  if (!(key in out)) {
    return;
  }
  out[key as keyof Permissions][mode] = list;
}

function normalizePermissions(value: unknown): Permissions {
  const out = emptyPermissions();
  if (Array.isArray(value)) {
    // Legacy: ["net", "env", "--allow-read=/tmp", "--deny-read=/secret", "all"]
    for (const item of asStringArray(value)) {
      const trimmed = item.trim();
      if (!trimmed) {
        continue;
      }
      if (trimmed === "all") {
        for (const key of CAPABILITY_KEYS) {
          out[key].allow = ["*"];
        }
        continue;
      }
      let matchedFlag = false;
      for (const mode of ["allow", "deny"] as const) {
        const prefix = `--${mode}-`;
        if (trimmed.startsWith(prefix)) {
          const body = trimmed.slice(prefix.length);
          const eq = body.indexOf("=");
          const key = (eq === -1 ? body : body.slice(0, eq)).toLowerCase();
          const list = eq === -1
            ? ["*"]
            : body.slice(eq + 1).split(",").map((s) => s.trim()).filter(Boolean);
          applyFlagList(out, mode, key, list);
          matchedFlag = true;
          break;
        }
      }
      if (matchedFlag) {
        continue;
      }
      const key = trimmed.toLowerCase();
      if ((CAPABILITY_KEYS as readonly string[]).includes(key)) {
        out[key as keyof Permissions].allow = ["*"];
      }
    }
    return out;
  }
  if (!isObject(value)) {
    return out;
  }
  for (const key of CAPABILITY_KEYS) {
    out[key] = normalizeCapability(value[key]);
  }
  return out;
}

function normalizeAnnotations(value: unknown): Annotations | undefined {
  if (!isObject(value)) {
    return undefined;
  }
  const annotations: Annotations = {};
  if (typeof value.readOnlyHint === "boolean") {
    annotations.readOnlyHint = value.readOnlyHint;
  }
  if (typeof value.destructiveHint === "boolean") {
    annotations.destructiveHint = value.destructiveHint;
  }
  if (typeof value.idempotentHint === "boolean") {
    annotations.idempotentHint = value.idempotentHint;
  }
  if (typeof value.openWorldHint === "boolean") {
    annotations.openWorldHint = value.openWorldHint;
  }
  return Object.keys(annotations).length > 0 ? annotations : undefined;
}

function formatError(error: unknown, seen = new Set<unknown>()): FormattedError {
  if (error === undefined) {
    return { name: "Error", message: "undefined" };
  }
  if (error === null) {
    return { name: "Error", message: "null" };
  }
  if (typeof error === "string") {
    return { name: "Error", message: error };
  }
  if (typeof error === "number" || typeof error === "boolean" || typeof error === "bigint") {
    return { name: "Error", message: String(error) };
  }
  if (typeof error !== "object") {
    return { name: "Error", message: String(error) };
  }
  if (seen.has(error)) {
    return { name: "Error", message: "[circular]" };
  }
  seen.add(error);

  if (error instanceof AggregateError) {
    const parts = error.errors.map((item) => formatError(item, seen).message).filter(Boolean);
    return {
      name: error.name || "AggregateError",
      message: parts.length > 0 ? parts.join("; ") : (error.message || "multiple errors"),
      stack: error.stack,
    };
  }

  if (error instanceof Error) {
    let message = error.message || error.name || "Error";
    if ("cause" in error && error.cause !== undefined && error.cause !== null) {
      const cause = formatError(error.cause, seen);
      message = `${message}: ${cause.name}: ${cause.message}`;
    }
    return {
      name: error.name || "Error",
      message,
      stack: error.stack,
    };
  }

  const record = error as Record<string, unknown>;
  const name = typeof record.name === "string" && record.name ? record.name : "Error";
  if (typeof record.message === "string" && record.message) {
    return { name, message: record.message, stack: typeof record.stack === "string" ? record.stack : undefined };
  }
  try {
    return { name, message: JSON.stringify(error) };
  } catch {
    return { name, message: Object.prototype.toString.call(error) };
  }
}

function jsonTypeOf(value: unknown): string {
  if (value === null) {
    return "null";
  }
  if (Array.isArray(value)) {
    return "array";
  }
  return typeof value;
}

function validateAgainstSchema(value: unknown, schema: Record<string, unknown>, path: string): void {
  if (typeof schema.type === "string") {
    const actual = jsonTypeOf(value);
    const expected = schema.type;
    const ok =
      expected === actual ||
      (expected === "integer" && actual === "number" && Number.isInteger(value as number));
    if (!ok) {
      throw new Error(`${path}: expected ${expected}, got ${actual}`);
    }
  }

  if (schema.type === "object" || isObject(schema.properties) || Array.isArray(schema.required)) {
    if (!isObject(value)) {
      if (schema.type === "object") {
        throw new Error(`${path}: expected object, got ${jsonTypeOf(value)}`);
      }
      return;
    }
    const required = asStringArray(schema.required);
    for (const key of required) {
      if (!(key in value)) {
        throw new Error(`${path}: missing required property "${key}"`);
      }
    }
    if (isObject(schema.properties)) {
      for (const [key, propSchema] of Object.entries(schema.properties)) {
        if (key in value && isObject(propSchema)) {
          validateAgainstSchema(value[key], propSchema, `${path}.${key}`);
        }
      }
    }
  }

  if (schema.type === "array" && Array.isArray(value) && isObject(schema.items)) {
    value.forEach((item, index) => {
      validateAgainstSchema(item, schema.items as Record<string, unknown>, `${path}[${index}]`);
    });
  }
}

function normalize(mod: Record<string, unknown>, scriptPath: string): ToolSpec {
  const def = mod.default;
  if (def && typeof def === "object") {
    const obj = def as Record<string, unknown>;
    const run = obj.run ?? obj.handler;
    if (typeof run === "function") {
      const outputSchema = isObject(obj.outputSchema) ? obj.outputSchema : undefined;
      const annotations = normalizeAnnotations(obj.annotations);
      return {
        name: typeof obj.name === "string" && obj.name ? obj.name : stem(scriptPath),
        description: typeof obj.description === "string" ? obj.description : "",
        inputSchema: isObject(obj.inputSchema) ? obj.inputSchema : emptySchema(),
        ...(outputSchema ? { outputSchema } : {}),
        permissions: normalizePermissions(obj.permissions ?? mod.permissions),
        ...(annotations ? { annotations } : {}),
        run: run as (args: unknown) => unknown,
      };
    }
  }

  const run = typeof def === "function" ? def : (mod.run ?? mod.handler);
  if (typeof run !== "function") {
    throw new Error(
      "script must default-export an object with run(), or a function",
    );
  }

  const outputSchema = isObject(mod.outputSchema) ? mod.outputSchema : undefined;
  const annotations = normalizeAnnotations(mod.annotations);
  return {
    name: typeof mod.name === "string" && mod.name ? mod.name : stem(scriptPath),
    description: typeof mod.description === "string" ? mod.description : "",
    inputSchema: isObject(mod.inputSchema) ? mod.inputSchema : emptySchema(),
    ...(outputSchema ? { outputSchema } : {}),
    permissions: normalizePermissions(mod.permissions),
    ...(annotations ? { annotations } : {}),
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
      ...(tool.outputSchema ? { outputSchema: tool.outputSchema } : {}),
      permissions: tool.permissions,
      ...(tool.annotations ? { annotations: tool.annotations } : {}),
    });
    return;
  }

  const raw = (await readStdin()).trim();
  let args: unknown = {};
  if (raw) {
    try {
      args = JSON.parse(raw);
    } catch (error) {
      throw new Error(`invalid tool arguments JSON: ${formatError(error).message}`);
    }
  }

  validateAgainstSchema(args, tool.inputSchema, "args");

  const result = await tool.run(args);
  writeJson({ ok: true, result });
}

try {
  await main();
} catch (error) {
  writeJson({ ok: false, error: formatError(error) });
}
