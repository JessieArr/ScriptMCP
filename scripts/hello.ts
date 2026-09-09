export const name = "hello";
export const description = "Greet someone by name.";
export const inputSchema = {
  type: "object",
  properties: {
    name: {
      type: "string",
      description: "Name to greet",
    },
  },
  required: ["name"],
};

export default function hello({ name }: { name: string }): string {
  return `Hello, ${name}!`;
}
