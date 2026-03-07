const express = require("express");
const app = express();
const port = process.env.PORT || 3003;

app.use(express.raw({ type: "application/json", limit: "1mb" }));

// ─── Shared data ────────────────────────────────────────────────────
const worlds = Array.from({ length: 10000 }, (_, i) => ({
  id: i + 1,
  randomNumber: Math.floor(Math.random() * 10000) + 1,
}));

const fortunes = [
  { id: 1, message: 'fortune: No such file or directory' },
  { id: 2, message: "A computer scientist is someone who fixes things that aren't broken." },
  { id: 3, message: 'After all is said and done, more is said than done.' },
  { id: 4, message: 'Any program that runs right is obsolete.' },
  { id: 5, message: 'A list is only as strong as its weakest link. \u2014 Donald Knuth' },
  { id: 6, message: 'Feature: A bug with seniority.' },
  { id: 7, message: 'Computers make very fast, very accurate mistakes.' },
  { id: 8, message: '<script>alert("This should not be displayed in a browser alert box.");</script>' },
  { id: 9, message: 'A computer program does what you tell it to do, not what you want it to do.' },
  { id: 10, message: 'If Java had true garbage collection, most programs would delete themselves upon execution.' },
  { id: 11, message: '\u30d5\u30ec\u30fc\u30e0\u30ef\u30fc\u30af\u306e\u30d9\u30f3\u30c1\u30de\u30fc\u30af' },
  { id: 12, message: 'The best thing about a boolean is even if you are wrong, you are only off by a bit.' },
];

const users = Array.from({ length: 1000 }, (_, i) => ({
  id: i + 1,
  name: `User ${i + 1}`,
  email: `user${i + 1}@example.com`,
  role: (i + 1) % 10 === 0 ? "admin" : "member",
}));

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#x27;");
}

function parseCount(val) {
  const n = parseInt(val, 10);
  return isNaN(n) ? 1 : Math.min(Math.max(n, 1), 500);
}

function randomWorld() {
  return worlds[Math.floor(Math.random() * worlds.length)];
}

// ─── TechEmpower ────────────────────────────────────────────────────

app.get("/plaintext", (_req, res) => {
  res.set("Content-Type", "text/plain").send("Hello, World!");
});

app.get("/json", (_req, res) => {
  res.json({ message: "Hello, World!" });
});

app.get("/db", (_req, res) => {
  res.json(randomWorld());
});

app.get("/queries", (req, res) => {
  const count = parseCount(req.query.queries);
  const results = Array.from({ length: count }, () => randomWorld());
  res.json(results);
});

app.get("/fortunes", (_req, res) => {
  const rows = [...fortunes.map((f) => [f.id, f.message])];
  rows.push([0, "Additional fortune added at request time."]);
  rows.sort((a, b) => a[1].localeCompare(b[1]));
  let html = '<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>';
  for (const [id, msg] of rows) {
    html += `<tr><td>${id}</td><td>${escapeHtml(msg)}</td></tr>`;
  }
  html += "</table></body></html>";
  res.set("Content-Type", "text/html; charset=utf-8").send(html);
});

app.get("/updates", (req, res) => {
  const count = parseCount(req.query.queries);
  const results = Array.from({ length: count }, () => {
    const w = { ...randomWorld() };
    w.randomNumber = Math.floor(Math.random() * 10000) + 1;
    return w;
  });
  res.json(results);
});

app.get("/cached-queries", (req, res) => {
  const count = parseCount(req.query.count);
  const results = Array.from({ length: count }, () => randomWorld());
  res.json(results);
});

// ─── REST API ───────────────────────────────────────────────────────

app.get("/api/users/:id", (req, res) => {
  const id = parseInt(req.params.id, 10);
  const user = users.find((u) => u.id === id);
  if (user) {
    res.json(user);
  } else {
    res.status(404).json({ error: "not found" });
  }
});

app.get("/api/search", (req, res) => {
  const q = req.query.q || "";
  const page = parseInt(req.query.page, 10) || 1;
  const limit = Math.min(parseInt(req.query.limit, 10) || 20, 100);
  const offset = (page - 1) * limit;
  const results = users
    .filter((u) => !q || u.name.includes(q))
    .slice(offset, offset + limit);
  res.json(results);
});

app.post("/api/echo", (req, res) => {
  res.set("Content-Type", "application/json").send(req.body);
});

// ─── Browser / misc ─────────────────────────────────────────────────

app.get("/browser/page", (_req, res) => {
  res.set("Content-Type", "text/html; charset=utf-8").send("<html><body><h1>Hello</h1></body></html>");
});

app.get("/redirect", (_req, res) => {
  res.redirect(302, "/plaintext");
});

app.listen(port, () => {
  console.error(`Express listening on 0.0.0.0:${port}`);
});
