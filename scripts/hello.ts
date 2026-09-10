export default {
  name: "hello",
  description: "Greet someone by name.",
  inputSchema: {
    type: "object",
    properties: {
      name: {
        type: "string",
        description: "Name to greet",
      },
    },
    required: ["name"],
  },
  async run(args: { name: string }) {
    return `Hello, ${args.name}!`;
  },
};
