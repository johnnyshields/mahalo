"""
ASGI app for benchmarking with Granian (or uvicorn).

Run with:
  granian --interface asgi --host 0.0.0.0 --port 3008 --workers 4 app:app
  # or: uvicorn app:app --host 0.0.0.0 --port 3008 --workers 4
"""

import json
import html
import os
import random
from urllib.parse import parse_qs

# ─── Shared data ─────────────────────────────────────────────────────

WORLDS = [{"id": i, "randomNumber": random.randint(1, 10000)} for i in range(1, 10001)]

FORTUNES = [
    {"id": 1,  "message": "fortune: No such file or directory"},
    {"id": 2,  "message": "A computer scientist is someone who fixes things that aren't broken."},
    {"id": 3,  "message": "After all is said and done, more is said than done."},
    {"id": 4,  "message": "Any program that runs right is obsolete."},
    {"id": 5,  "message": "A list is only as strong as its weakest link. — Donald Knuth"},
    {"id": 6,  "message": "Feature: A bug with seniority."},
    {"id": 7,  "message": "Computers make very fast, very accurate mistakes."},
    {"id": 8,  "message": '<script>alert("This should not be displayed in a browser alert box.");</script>'},
    {"id": 9,  "message": "A computer program does what you tell it to do, not what you want it to do."},
    {"id": 10, "message": "If Java had true garbage collection, most programs would delete themselves upon execution."},
    {"id": 11, "message": "フレームワークのベンチマーク"},
    {"id": 12, "message": "The best thing about a boolean is even if you are wrong, you are only off by a bit."},
]

USERS = [
    {"id": i, "name": f"User {i}", "email": f"user{i}@example.com",
     "role": "admin" if i % 10 == 0 else "member"}
    for i in range(1, 1001)
]


def parse_count(val, default=1):
    try:
        return max(1, min(int(val), 500))
    except (TypeError, ValueError):
        return default


def render_fortunes():
    rows = [(f["id"], f["message"]) for f in FORTUNES]
    rows.append((0, "Additional fortune added at request time."))
    rows.sort(key=lambda r: r[1])
    parts = ['<!DOCTYPE html><html><head><title>Fortunes</title></head><body><table><tr><th>id</th><th>message</th></tr>']
    for fid, msg in rows:
        parts.append(f"<tr><td>{fid}</td><td>{html.escape(msg)}</td></tr>")
    parts.append("</table></body></html>")
    return "".join(parts)


# ─── ASGI app ────────────────────────────────────────────────────────

async def app(scope, receive, send):
    if scope["type"] != "http":
        return

    path = scope["path"]
    method = scope["method"]
    qs = parse_qs(scope.get("query_string", b"").decode())

    # Helper
    async def respond(status, headers, body):
        await send({"type": "http.response.start", "status": status, "headers": headers})
        await send({"type": "http.response.body", "body": body if isinstance(body, bytes) else body.encode()})

    json_hdr = [(b"content-type", b"application/json")]
    text_hdr = [(b"content-type", b"text/plain")]
    html_hdr = [(b"content-type", b"text/html; charset=utf-8")]

    # ── TechEmpower ──────────────────────────────────────────────

    if path == "/plaintext" and method == "GET":
        await respond(200, text_hdr, b"Hello, World!")

    elif path == "/json" and method == "GET":
        await respond(200, json_hdr, json.dumps({"message": "Hello, World!"}))

    elif path == "/db" and method == "GET":
        await respond(200, json_hdr, json.dumps(random.choice(WORLDS)))

    elif path == "/queries" and method == "GET":
        count = parse_count(qs.get("queries", ["1"])[0])
        results = [random.choice(WORLDS) for _ in range(count)]
        await respond(200, json_hdr, json.dumps(results))

    elif path == "/fortunes" and method == "GET":
        await respond(200, html_hdr, render_fortunes())

    elif path == "/updates" and method == "GET":
        count = parse_count(qs.get("queries", ["1"])[0])
        results = []
        for _ in range(count):
            w = dict(random.choice(WORLDS))
            w["randomNumber"] = random.randint(1, 10000)
            results.append(w)
        await respond(200, json_hdr, json.dumps(results))

    elif path == "/cached-queries" and method == "GET":
        count = parse_count(qs.get("count", ["1"])[0])
        results = [random.choice(WORLDS) for _ in range(count)]
        await respond(200, json_hdr, json.dumps(results))

    # ── REST API ─────────────────────────────────────────────────

    elif path.startswith("/api/users/") and method == "GET":
        try:
            uid = int(path.split("/")[-1])
            user = next((u for u in USERS if u["id"] == uid), None)
            if user:
                await respond(200, json_hdr, json.dumps(user))
            else:
                await respond(404, json_hdr, b'{"error":"not found"}')
        except ValueError:
            await respond(404, json_hdr, b'{"error":"not found"}')

    elif path == "/api/search" and method == "GET":
        q = qs.get("q", [""])[0]
        page = parse_count(qs.get("page", ["1"])[0])
        limit = min(parse_count(qs.get("limit", ["20"])[0]), 100)
        offset = (page - 1) * limit
        results = [u for u in USERS if not q or q in u["name"]]
        results = results[offset:offset + limit]
        await respond(200, json_hdr, json.dumps(results))

    elif path == "/api/echo" and method == "POST":
        body = b""
        while True:
            msg = await receive()
            body += msg.get("body", b"")
            if not msg.get("more_body", False):
                break
        await respond(200, json_hdr, body)

    # ── Browser / misc ───────────────────────────────────────────

    elif path == "/browser/page" and method == "GET":
        await respond(200, html_hdr, b"<html><body><h1>Hello</h1></body></html>")

    elif path == "/redirect" and method == "GET":
        await send({"type": "http.response.start", "status": 302, "headers": [(b"location", b"/plaintext")]})
        await send({"type": "http.response.body", "body": b""})

    else:
        await respond(404, text_hdr, b"Not Found")
