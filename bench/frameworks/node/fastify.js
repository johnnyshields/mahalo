const fastify = require("fastify")({ logger: false });
const port = process.env.PORT || 3004;

fastify.get("/plaintext", async (_req, reply) => {
  reply.type("text/plain").send("Hello, World!");
});

fastify.get("/json", async (_req, reply) => {
  return { message: "Hello, World!" };
});

fastify.listen({ port, host: "0.0.0.0" }).then(() => {
  console.error(`Fastify listening on 0.0.0.0:${port}`);
}).catch((err) => {
  console.error("Fastify startup error:", err);
  process.exit(1);
});
