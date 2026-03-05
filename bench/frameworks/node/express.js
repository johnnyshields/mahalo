const express = require("express");
const app = express();
const port = process.env.PORT || 3003;

app.get("/plaintext", (_req, res) => {
  res.set("Content-Type", "text/plain").send("Hello, World!");
});

app.get("/json", (_req, res) => {
  res.json({ message: "Hello, World!" });
});

app.listen(port, () => {
  console.error(`Express listening on 0.0.0.0:${port}`);
});
