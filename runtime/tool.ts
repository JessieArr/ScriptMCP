/**
 * ScriptMCP tool module contract.
 *
 * Each tool is a TypeScript/JavaScript module that default-exports a plain
 * object matching {@link ScriptTool}. Scripts do not depend on an MCP SDK.
 */

/** JSON Schema object (subset used by tools). */
export type JSONSchema = Record<string, unknown>;

/**
 * Allow/deny lists for one Deno capability.
 *
 * A bare `string[]` is shorthand for `{ allow: [...] }`.
 */
export type CapabilityPermission =
  | string[]
  | {
      allow?: string[];
      deny?: string[];
    };

/** Deno capabilities requested by the script. */
export type ScriptPermissions = {
  read?: CapabilityPermission;
  write?: CapabilityPermission;
  net?: CapabilityPermission;
  env?: CapabilityPermission;
  run?: CapabilityPermission;
  sys?: CapabilityPermission;
};

/** Standard MCP tool behavior hints. */
export type ScriptAnnotations = {
  readOnlyHint?: boolean;
  destructiveHint?: boolean;
  idempotentHint?: boolean;
  openWorldHint?: boolean;
};

/** Default-export shape for a ScriptMCP tool module. */
export type ScriptTool = {
  /** MCP tool name. Defaults to the file stem when omitted. */
  name: string;
  /** Exposed to the LLM: what the tool does and when to use it. */
  description: string;
  /** JSON Schema for arguments passed to `run()`. */
  inputSchema?: JSONSchema;
  /** Optional JSON Schema for the structured value returned by `run()`. */
  outputSchema?: JSONSchema;
  /**
   * Deno capabilities. Paths may use `${workspace}` (scripts directory).
   * Use `"*"` for an unrestricted capability flag. Prefer `{ allow, deny }`
   * when you need both grants and exclusions.
   */
  permissions?: ScriptPermissions;
  /** MCP tool behavior hints. */
  annotations?: ScriptAnnotations;
  /** Tool implementation. The host validates `args` against `inputSchema`. */
  run: (args: any) => any | Promise<any>;
};
