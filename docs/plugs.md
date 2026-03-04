# Plugs and Pipelines Guide

Plugs are Mahalo's composable middleware abstraction. Every request transformation -- authentication, logging, content-type headers, CORS -- is a plug.

## The Plug Trait

```rust
pub trait Plug: Send + Sync + 'static {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn>;
}
```

A plug receives a `Conn`, does work, and returns the (possibly modified) `Conn`.

## Creating Plugs

### From Async Functions (most common)

Use `plug_fn()` to wrap any async function:

```rust
use mahalo::{Conn, plug_fn};
use http::StatusCode;

let json_header = plug_fn(|conn: Conn| async {
    conn.put_resp_header("content-type", "application/json")
});

let auth_check = plug_fn(|conn: Conn| async {
    match conn.headers.get("authorization") {
        Some(_) => conn,
        None => conn.put_status(StatusCode::UNAUTHORIZED).halt(),
    }
});
```

### From a Struct (for plugs with configuration)

Implement `Plug` directly on a struct:

```rust
use mahalo::{Conn, Plug, BoxFuture};

struct CorsPlug {
    allowed_origin: String,
}

impl Plug for CorsPlug {
    fn call(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        let origin = self.allowed_origin.clone();
        Box::pin(async move {
            conn.put_resp_header("access-control-allow-origin", origin)
        })
    }
}

// Usage
let cors = CorsPlug { allowed_origin: "*".to_string() };
```

## Halting

Any plug can halt the pipeline by calling `conn.halt()`. Subsequent plugs and the route handler will be skipped:

```rust
let rate_limiter = plug_fn(|conn: Conn| async {
    if is_rate_limited(&conn) {
        return conn
            .put_status(StatusCode::TOO_MANY_REQUESTS)
            .put_resp_body("Rate limited")
            .halt();
    }
    conn
});
```

## Pipelines

A `Pipeline` is a named, ordered sequence of plugs:

```rust
use mahalo::Pipeline;

let api_pipeline = Pipeline::new("api")
    .plug(json_header)
    .plug(auth_check)
    .plug(rate_limiter);
```

### Execution

```rust
let conn = pipeline.execute(conn).await;
```

Plugs run in order. If any plug halts the conn, remaining plugs are skipped.

### With the Router

Register pipelines on the router and apply them to scopes:

```rust
let router = MahaloRouter::new()
    .pipeline(api_pipeline)
    .pipeline(browser_pipeline)
    .scope("/api", &["api"], |s| {
        // All routes here run through the "api" pipeline first
        s.get("/users", list_users);
    })
    .scope("/", &["browser"], |s| {
        // These routes run through the "browser" pipeline
        s.get("/", home_page);
    });
```

Multiple pipelines can be applied to a scope:

```rust
.scope("/admin", &["api", "admin_auth"], |s| { /* ... */ })
```

They execute in the order listed.

## Common Plug Patterns

### Request Logging

```rust
let logger = plug_fn(|conn: Conn| async {
    tracing::info!(
        method = %conn.method,
        path = %conn.uri.path(),
        "Incoming request"
    );
    conn
});
```

### Content-Type Enforcement

```rust
let require_json = plug_fn(|conn: Conn| async {
    match conn.headers.get("content-type") {
        Some(ct) if ct == "application/json" => conn,
        _ => conn
            .put_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            .halt(),
    }
});
```

### Assign Current User

```rust
struct CurrentUser;
impl AssignKey for CurrentUser {
    type Value = User;
}

let load_user = plug_fn(|conn: Conn| async {
    if let Some(user) = authenticate(&conn).await {
        conn.assign::<CurrentUser>(user)
    } else {
        conn.put_status(StatusCode::UNAUTHORIZED).halt()
    }
});
```
