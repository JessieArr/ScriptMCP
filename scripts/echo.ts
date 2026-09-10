export default {
  name: "echo",
  description: "Return the provided JSON payload unchanged.",
  inputSchema: {
    type: "object",
    properties: {
      value: {
        description: "Any JSON value to echo back",
      },
    },
    required: ["value"],
  },
  annotations: {
    readOnlyHint: true,
    idempotentHint: true,
  },
  async run(args: { value: unknown }) {
    return {
      result: args.value,
    };
  },
};
