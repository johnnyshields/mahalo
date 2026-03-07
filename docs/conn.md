# Conn Reference

`Conn` is the central request + response struct in Mahalo, inspired by Phoenix's `Plug.Conn`. A single `Conn` flows through the entire request pipeline, accumulating request data and building the response.

## Creating a Conn

```rust
use mahalo::Conn;
use http::{Method, Uri};

let conn = Conn::new(Method::GET, Uri::from_static("/api/rooms"));
```

In production, `Conn` is created by the monoio server from the parsed HTTP/1.1 request. You'll typically create `Conn` directly only in tests.

## Builder Methods

All builder methods consume and return `self` for chaining:

```rust
let conn = Conn::new(Method::GET, Uri::from_static("/"))
    .put_status(StatusCode::OK)
    .put_resp_header("content-type", "application/json")
    .put_resp_body(r#"{"ok":true}"#)
    .halt();  // stops pipeline execution
```

### `put_status(status: StatusCode) -> Self`

Set the response HTTP status code.

### `put_resp_header(key, value) -> Self`

Add a response header. Key and value must be convertible to `HeaderName` / `HeaderValue`.

### `put_resp_body(body: impl Into<Bytes>) -> Self`

Set the response body. Accepts `&str`, `String`, `Vec<u8>`, `Bytes`, etc.

### `halt() -> Self`

Mark the connection as halted. Pipelines will stop executing further plugs when `conn.halted` is true.

### `assign<K: AssignKey>(value: K::Value) -> Self`

Store a typed value in the assigns map. See [Typed Assigns](#typed-assigns) below.

### `with_runtime(runtime: Rc<Runtime>) -> Self`

Attach a rebar runtime reference to the connection.

## Reading State

### `get_assign<K: AssignKey>() -> Option<&K::Value>`

Retrieve a typed value from the assigns map by key type.

### `parse_query_params()`

Parse the URI query string into `query_params`. Called automatically by `MahaloEndpoint`. Values are percent-decoded (e.g. `%20` becomes a space).

## Public Fields

```rust
// Request data (populated by MahaloEndpoint)
conn.method         // http::Method
conn.uri            // http::Uri
conn.headers        // http::HeaderMap
conn.path_params    // HashMap<String, String> - from route :params
conn.query_params   // HashMap<String, String> - from ?key=value
conn.remote_addr    // Option<SocketAddr>
conn.body           // bytes::Bytes

// Response data (set by your plugs/handlers)
conn.status         // http::StatusCode (default: 200 OK)
conn.resp_headers   // http::HeaderMap
conn.resp_body      // bytes::Bytes

// Control flow
conn.halted         // bool - if true, pipeline stops
conn.runtime        // Option<Rc<Runtime>> - rebar runtime
```

## Typed Assigns

Assigns provide type-safe per-request state storage. Define a key type, implement `AssignKey`, then use `assign()` and `get_assign()`:

```rust
use mahalo::{AssignKey, Conn};

// Define a key (zero-sized type)
struct CurrentUser;
impl AssignKey for CurrentUser {
    type Value = User;
}

// Store
let conn = conn.assign::<CurrentUser>(user);

// Retrieve
if let Some(user) = conn.get_assign::<CurrentUser>() {
    println!("Hello, {}", user.name);
}
```

Multiple keys can coexist. Each key type maps to exactly one value. Assigning to the same key overwrites the previous value.

## In Plugs

A plug receives a `Conn` and returns a (potentially modified) `Conn`:

```rust
use mahalo::{Conn, plug_fn};
use http::StatusCode;

let my_plug = plug_fn(|conn: Conn| async {
    if conn.headers.get("authorization").is_none() {
        return conn.put_status(StatusCode::UNAUTHORIZED).halt();
    }
    conn
});
```

## In Controllers

Controller actions receive `Conn` by value and return it wrapped in `BoxFuture`:

```rust
use mahalo::{Conn, Controller, BoxFuture};
use http::StatusCode;

struct MyController;

impl Controller for MyController {
    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = conn.path_params.get("id").cloned().unwrap_or_default();
            conn.put_status(StatusCode::OK)
                .put_resp_body(format!("Showing item {id}"))
        })
    }
}
```
