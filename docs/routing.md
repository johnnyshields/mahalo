# Routing Guide

Mahalo's router matches HTTP requests to handlers using method + path pattern matching, with support for scopes, named pipelines, path parameters, and RESTful resources.

## Basic Routes

```rust
use mahalo::{Conn, MahaloRouter, plug_fn};
use http::StatusCode;

let router = MahaloRouter::new()
    .get("/health", plug_fn(|conn: Conn| async {
        conn.put_status(StatusCode::OK).put_resp_body("ok")
    }))
    .post("/submit", plug_fn(|conn: Conn| async {
        conn.put_status(StatusCode::CREATED).put_resp_body("created")
    }));
```

Top-level routes have no pipeline and no path prefix.

## Path Parameters

Use `:name` segments to capture dynamic path parts:

```rust
router.scope("/api", &[], |s| {
    // GET /api/users/:id → path_params["id"] = "42"
    s.get("/users/:id", plug_fn(|conn: Conn| async {
        let id = conn.path_params.get("id").unwrap();
        conn.put_resp_body(format!("User {id}"))
    }));
});
```

Parameters are extracted during route resolution and stored in `conn.path_params`.

## Scopes

Scopes group routes under a path prefix and optionally apply named pipelines:

```rust
let router = MahaloRouter::new()
    .pipeline(api_pipeline)
    .scope("/api", &["api"], |s| {
        s.get("/users", handler);        // GET /api/users
        s.post("/users", handler);       // POST /api/users
        s.get("/users/:id", handler);    // GET /api/users/:id
        s.put("/users/:id", handler);    // PUT /api/users/:id
        s.delete("/users/:id", handler); // DELETE /api/users/:id
    });
```

### ScopeBuilder Methods

| Method | HTTP Method |
|--------|------------|
| `s.get(path, plug)` | GET |
| `s.post(path, plug)` | POST |
| `s.put(path, plug)` | PUT |
| `s.patch(path, plug)` | PATCH |
| `s.delete(path, plug)` | DELETE |
| `s.resources(path, controller)` | All CRUD methods |

## Pipelines

Pipelines are named, ordered sequences of plugs:

```rust
use mahalo::Pipeline;

let api_pipeline = Pipeline::new("api")
    .plug(plug_fn(|conn: Conn| async {
        conn.put_resp_header("content-type", "application/json")
    }))
    .plug(plug_fn(|conn: Conn| async {
        // Authentication check
        if conn.headers.get("authorization").is_none() {
            return conn.put_status(StatusCode::UNAUTHORIZED).halt();
        }
        conn
    }));
```

When a pipeline plug calls `halt()`, the remaining plugs and the route handler are skipped. The response is sent as-is.

Register pipelines on the router and reference them by name in scopes:

```rust
let router = MahaloRouter::new()
    .pipeline(api_pipeline)
    .pipeline(browser_pipeline)
    .scope("/api", &["api"], |s| { /* ... */ })
    .scope("/", &["browser"], |s| { /* ... */ });
```

## RESTful Resources

Generate standard CRUD routes for a `Controller`:

```rust
s.resources("/rooms", RoomController);
```

This generates:

| Method | Path | Action |
|--------|------|--------|
| GET | `/rooms` | `index` |
| GET | `/rooms/:id` | `show` |
| POST | `/rooms` | `create` |
| PUT | `/rooms/:id` | `update` |
| DELETE | `/rooms/:id` | `delete` |

Unimplemented actions return 404 by default (from the `Controller` trait default implementations).

## Route Resolution

Routes are matched in registration order. The first route matching both the HTTP method and path pattern wins.

```rust
// Resolution
let resolved = router.resolve(&Method::GET, "/api/rooms/42");

if let Some(route) = resolved {
    // route.path_params: {"id": "42"}
    // route.pipelines: [&api_pipeline]
    let conn = route.execute(conn).await;
}
```

## Query Parameters

Query parameters are automatically parsed and URL-decoded by `Conn::parse_query_params()` (called by `MahaloEndpoint`):

```rust
// GET /search?q=hello%20world&page=2

let q = conn.query_params.get("q");     // Some("hello world")
let page = conn.query_params.get("page"); // Some("2")
```
