use std::cell::OnceCell;
use std::collections::HashMap;
use std::rc::Rc;

use http::Method;
use mahalo_core::conn::{Conn, PathParams};
use mahalo_core::controller::Controller;
use mahalo_core::pipeline::Pipeline;
use mahalo_core::plug::Plug;
use smallvec::SmallVec;

/// What a matched route dispatches to.
enum Handler {
    /// A plug function (for individual route handlers).
    Plug(Box<dyn Plug>),
    /// A controller + action name (for resourceful routes).
    Controller {
        controller: Rc<dyn Controller>,
        action: String,
    },
}

/// An individual route entry.
struct Route {
    method: Method,
    /// Pattern segments, e.g. ["api", "rooms", ":id"] (used for path_for/named_routes)
    segments: Vec<String>,
    /// matchit-compatible path, e.g. "/api/rooms/{id}"
    matchit_path: String,
    handler: Handler,
    /// Names of pipelines to run before this route's handler.
    pipeline_names: Vec<String>,
    /// Optional name for reverse routing (URL helpers).
    name: Option<String>,
    /// Pre-extracted parameter names as &'static str (leaked once).
    /// Avoids per-request String allocation for param keys.
    param_keys: Vec<&'static str>,
}

/// Extract parameter names from segments and leak them as &'static str.
fn extract_param_keys(segments: &[String]) -> Vec<&'static str> {
    segments
        .iter()
        .filter_map(|s| s.strip_prefix(':'))
        .map(|name| -> &'static str { Box::leak(name.to_string().into_boxed_str()) })
        .collect()
}

/// Compiled per-method matchit routers. Each maps path → route index.
struct CompiledRouters {
    get: matchit::Router<usize>,
    post: matchit::Router<usize>,
    put: matchit::Router<usize>,
    patch: matchit::Router<usize>,
    delete: matchit::Router<usize>,
}

impl CompiledRouters {
    fn for_method(&self, method: &Method) -> Option<&matchit::Router<usize>> {
        match *method {
            Method::GET | Method::HEAD => Some(&self.get),
            Method::POST => Some(&self.post),
            Method::PUT => Some(&self.put),
            Method::PATCH => Some(&self.patch),
            Method::DELETE => Some(&self.delete),
            _ => None,
        }
    }
}

/// The result of resolving an incoming request to a route.
pub struct ResolvedRoute<'a> {
    pub path_params: PathParams,
    pub pipelines: Vec<&'a Pipeline>,
    handler: &'a Handler,
}

impl<'a> ResolvedRoute<'a> {
    /// Execute the resolved route: run pipelines then the handler.
    #[inline]
    pub async fn execute(self, mut conn: Conn) -> Conn {
        conn.path_params = self.path_params;

        // Run pipelines in order.
        for pipeline in &self.pipelines {
            if conn.halted {
                return conn;
            }
            conn = pipeline.execute(conn).await;
        }

        if conn.halted {
            return conn;
        }

        // Run the handler — try zero-alloc sync path first.
        match self.handler {
            Handler::Plug(plug) => match plug.call_sync(conn) {
                Ok(c) => c,
                Err(c) => plug.call(c).await,
            },
            Handler::Controller { controller, action } => {
                controller.call_action(action, conn).await
            }
        }
    }
}

/// Builder used inside a `scope()` closure to register routes.
pub struct ScopeBuilder {
    prefix: String,
    prefix_segments: Vec<String>,
    pipeline_names: Vec<String>,
    routes: Vec<Route>,
}

impl ScopeBuilder {
    fn new(prefix: String, prefix_segments: Vec<String>, pipeline_names: Vec<String>) -> Self {
        Self {
            prefix,
            prefix_segments,
            pipeline_names,
            routes: Vec::new(),
        }
    }

    fn add_route(&mut self, method: Method, path: &str, handler: Handler) {
        let mut segments = self.prefix_segments.clone();
        segments.extend(parse_segments(path));
        let matchit_path = build_matchit_path(&self.prefix, path);
        let param_keys = extract_param_keys(&segments);
        self.routes.push(Route {
            method,
            segments,
            matchit_path,
            handler,
            pipeline_names: self.pipeline_names.clone(),
            name: None,
            param_keys,
        });
    }

    fn add_named_route(&mut self, method: Method, path: &str, name: &str, handler: Handler) {
        let mut segments = self.prefix_segments.clone();
        segments.extend(parse_segments(path));
        let matchit_path = build_matchit_path(&self.prefix, path);
        let param_keys = extract_param_keys(&segments);
        self.routes.push(Route {
            method,
            segments,
            matchit_path,
            handler,
            pipeline_names: self.pipeline_names.clone(),
            name: Some(name.to_string()),
            param_keys,
        });
    }

    pub fn get(&mut self, path: &str, plug: impl Plug) {
        self.add_route(Method::GET, path, Handler::Plug(Box::new(plug)));
    }

    pub fn post(&mut self, path: &str, plug: impl Plug) {
        self.add_route(Method::POST, path, Handler::Plug(Box::new(plug)));
    }

    pub fn put(&mut self, path: &str, plug: impl Plug) {
        self.add_route(Method::PUT, path, Handler::Plug(Box::new(plug)));
    }

    pub fn patch(&mut self, path: &str, plug: impl Plug) {
        self.add_route(Method::PATCH, path, Handler::Plug(Box::new(plug)));
    }

    pub fn delete(&mut self, path: &str, plug: impl Plug) {
        self.add_route(Method::DELETE, path, Handler::Plug(Box::new(plug)));
    }

    pub fn get_named(&mut self, path: &str, name: &str, plug: impl Plug) {
        self.add_named_route(Method::GET, path, name, Handler::Plug(Box::new(plug)));
    }

    pub fn post_named(&mut self, path: &str, name: &str, plug: impl Plug) {
        self.add_named_route(Method::POST, path, name, Handler::Plug(Box::new(plug)));
    }

    pub fn put_named(&mut self, path: &str, name: &str, plug: impl Plug) {
        self.add_named_route(Method::PUT, path, name, Handler::Plug(Box::new(plug)));
    }

    pub fn patch_named(&mut self, path: &str, name: &str, plug: impl Plug) {
        self.add_named_route(Method::PATCH, path, name, Handler::Plug(Box::new(plug)));
    }

    pub fn delete_named(&mut self, path: &str, name: &str, plug: impl Plug) {
        self.add_named_route(Method::DELETE, path, name, Handler::Plug(Box::new(plug)));
    }

    /// Register standard CRUD routes for a controller:
    ///   GET    /path       -> index
    ///   GET    /path/:id   -> show
    ///   POST   /path       -> create
    ///   PUT    /path/:id   -> update
    ///   DELETE /path/:id   -> delete
    pub fn resources(&mut self, path: &str, controller: Rc<dyn Controller>) {
        let base = path;
        let with_id = format!("{}/:id", path);
        let prefix = path.trim_start_matches('/');

        self.add_named_route(
            Method::GET,
            base,
            &format!("{}_index", prefix),
            Handler::Controller {
                controller: Rc::clone(&controller),
                action: "index".to_string(),
            },
        );
        self.add_named_route(
            Method::GET,
            &with_id,
            &format!("{}_show", prefix),
            Handler::Controller {
                controller: Rc::clone(&controller),
                action: "show".to_string(),
            },
        );
        self.add_named_route(
            Method::POST,
            base,
            &format!("{}_create", prefix),
            Handler::Controller {
                controller: Rc::clone(&controller),
                action: "create".to_string(),
            },
        );
        self.add_named_route(
            Method::PUT,
            &with_id,
            &format!("{}_update", prefix),
            Handler::Controller {
                controller: Rc::clone(&controller),
                action: "update".to_string(),
            },
        );
        self.add_named_route(
            Method::DELETE,
            &with_id,
            &format!("{}_delete", prefix),
            Handler::Controller {
                controller: Rc::clone(&controller),
                action: "delete".to_string(),
            },
        );
    }
}

/// Top-level router holding named pipelines and routes.
/// Uses matchit radix trie for O(log N) route resolution.
pub struct MahaloRouter {
    pipelines: HashMap<String, Pipeline>,
    routes: Vec<Route>,
    name_index: HashMap<String, usize>,
    /// Lazily compiled matchit routers, one per HTTP method.
    compiled: OnceCell<CompiledRouters>,
}

impl MahaloRouter {
    pub fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            routes: Vec::new(),
            name_index: HashMap::new(),
            compiled: OnceCell::new(),
        }
    }

    /// Register a named pipeline.
    pub fn pipeline(mut self, p: Pipeline) -> Self {
        self.pipelines.insert(p.name.clone(), p);
        self
    }

    /// Add routes within a scoped prefix, applying the named pipelines.
    pub fn scope(
        mut self,
        prefix: &str,
        pipeline_names: &[&str],
        f: impl FnOnce(&mut ScopeBuilder),
    ) -> Self {
        let prefix_segments = parse_segments(prefix);
        let names: Vec<String> = pipeline_names.iter().map(|s| s.to_string()).collect();
        let prefix_str = normalize_prefix(prefix);
        let mut builder = ScopeBuilder::new(prefix_str, prefix_segments, names);
        f(&mut builder);
        let start = self.routes.len();
        self.routes.extend(builder.routes);
        for i in start..self.routes.len() {
            if let Some(ref name) = self.routes[i].name {
                self.name_index.insert(name.clone(), i);
            }
        }
        self
    }

    /// Add a top-level route (no scope, no pipelines).
    fn add_top_level_route(&mut self, method: Method, path: &str, plug: impl Plug, name: Option<&str>) {
        let segments = parse_segments(path);
        let param_keys = extract_param_keys(&segments);
        let idx = self.routes.len();
        self.routes.push(Route {
            method,
            segments,
            matchit_path: colon_to_brace(path),
            handler: Handler::Plug(Box::new(plug)),
            pipeline_names: Vec::new(),
            name: name.map(|n| n.to_string()),
            param_keys,
        });
        if let Some(n) = name {
            self.name_index.insert(n.to_string(), idx);
        }
    }

    /// Add a top-level GET route (no scope, no pipelines).
    pub fn get(mut self, path: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::GET, path, plug, None);
        self
    }

    /// Add a top-level POST route.
    pub fn post(mut self, path: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::POST, path, plug, None);
        self
    }

    /// Add a top-level named GET route.
    pub fn get_named(mut self, path: &str, name: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::GET, path, plug, Some(name));
        self
    }

    /// Add a top-level named POST route.
    pub fn post_named(mut self, path: &str, name: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::POST, path, plug, Some(name));
        self
    }

    /// Add a top-level PUT route.
    pub fn put(mut self, path: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::PUT, path, plug, None);
        self
    }

    /// Add a top-level PATCH route.
    pub fn patch(mut self, path: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::PATCH, path, plug, None);
        self
    }

    /// Add a top-level DELETE route.
    pub fn delete(mut self, path: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::DELETE, path, plug, None);
        self
    }

    /// Add a top-level named PUT route.
    pub fn put_named(mut self, path: &str, name: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::PUT, path, plug, Some(name));
        self
    }

    /// Add a top-level named PATCH route.
    pub fn patch_named(mut self, path: &str, name: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::PATCH, path, plug, Some(name));
        self
    }

    /// Add a top-level named DELETE route.
    pub fn delete_named(mut self, path: &str, name: &str, plug: impl Plug) -> Self {
        self.add_top_level_route(Method::DELETE, path, plug, Some(name));
        self
    }

    /// Reverse-route: build a path string from a named route and params.
    pub fn path_for(&self, name: &str, params: &[(&str, &str)]) -> Option<String> {
        let idx = self.name_index.get(name)?;
        let route = &self.routes[*idx];
        let mut parts = Vec::with_capacity(route.segments.len());
        for seg in &route.segments {
            if let Some(param_name) = seg.strip_prefix(':') {
                let value = params.iter().find(|(k, _)| *k == param_name)?.1;
                parts.push(value.to_string());
            } else {
                parts.push(seg.clone());
            }
        }
        Some(format!("/{}", parts.join("/")))
    }

    /// Return all named routes for introspection: (name, method, path_pattern).
    pub fn named_routes(&self) -> Vec<(String, Method, String)> {
        let mut result = Vec::new();
        for (name, idx) in &self.name_index {
            let route = &self.routes[*idx];
            let pattern = format!("/{}", route.segments.join("/"));
            result.push((name.clone(), route.method.clone(), pattern));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Compile the matchit routers from the route list (called once, lazily).
    fn compile(&self) -> CompiledRouters {
        let mut get = matchit::Router::new();
        let mut post = matchit::Router::new();
        let mut put = matchit::Router::new();
        let mut patch = matchit::Router::new();
        let mut delete = matchit::Router::new();

        for (idx, route) in self.routes.iter().enumerate() {
            let router = match route.method {
                Method::GET => &mut get,
                Method::POST => &mut post,
                Method::PUT => &mut put,
                Method::PATCH => &mut patch,
                Method::DELETE => &mut delete,
                _ => continue,
            };
            // matchit will panic on duplicate routes; that's a programming error
            router.insert(&route.matchit_path, idx).unwrap();
        }

        CompiledRouters {
            get,
            post,
            put,
            patch,
            delete,
        }
    }

    /// Resolve an incoming request method + path to a route.
    #[inline]
    pub fn resolve(&self, method: &Method, path: &str) -> Option<ResolvedRoute<'_>> {
        let compiled = self.compiled.get_or_init(|| self.compile());

        let method_router = compiled.for_method(method)?;
        let matched = method_router.at(path).ok()?;
        let route_idx = *matched.value;
        let route = &self.routes[route_idx];

        // Extract path params into SmallVec — zero heap allocation for 0-4 params.
        // Keys are leaked &'static str from route param names (allocated once at compile time).
        let mut path_params = PathParams::new();
        for (k, v) in matched.params.iter() {
            // Lookup the static key from the route's param_keys
            let static_key = route.param_keys.iter()
                .find(|pk| **pk == k)
                .copied()
                .unwrap_or_else(|| Box::leak(k.to_string().into_boxed_str()));
            path_params.push((static_key, SmallVec::from_slice(v.as_bytes())));
        }

        let pipelines: Vec<&Pipeline> = route
            .pipeline_names
            .iter()
            .filter_map(|name| self.pipelines.get(name))
            .collect();

        Some(ResolvedRoute {
            path_params,
            pipelines,
            handler: &route.handler,
        })
    }
}

impl Default for MahaloRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a path string into non-empty segments (preserves `:param` syntax).
fn parse_segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Convert `:param` segments to `{param}` for matchit.
/// e.g. "/api/rooms/:id" → "/api/rooms/{id}"
fn colon_to_brace(path: &str) -> String {
    let mut result = String::with_capacity(path.len() + 4);
    for segment in path.split('/') {
        if !result.is_empty() || path.starts_with('/') {
            if !result.is_empty() {
                result.push('/');
            } else {
                // First segment after leading /
            }
        }
        if let Some(param) = segment.strip_prefix(':') {
            result.push('{');
            result.push_str(param);
            result.push('}');
        } else {
            result.push_str(segment);
        }
    }
    // Ensure leading slash
    if !result.starts_with('/') {
        result.insert(0, '/');
    }
    result
}

/// Normalize a scope prefix: ensure leading slash, strip trailing slash.
fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    }
}

/// Build a matchit-compatible path from scope prefix + route path.
fn build_matchit_path(prefix: &str, path: &str) -> String {
    let full = if path == "/" || path.is_empty() {
        prefix.to_string()
    } else if path.starts_with('/') {
        format!("{}{}", prefix, path)
    } else {
        format!("{}/{}", prefix, path)
    };
    colon_to_brace(&full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{StatusCode, Uri};
    use mahalo_core::conn::Conn;
    use mahalo_core::pipeline::Pipeline;
    use mahalo_core::plug::{plug_fn, BoxFuture};

    #[test]
    fn parse_segments_works() {
        assert_eq!(parse_segments("/api/rooms"), vec!["api", "rooms"]);
        assert_eq!(parse_segments("/"), Vec::<String>::new());
        assert_eq!(parse_segments("/a/:id/b"), vec!["a", ":id", "b"]);
    }

    #[test]
    fn colon_to_brace_conversion() {
        assert_eq!(colon_to_brace("/health"), "/health");
        assert_eq!(colon_to_brace("/rooms/:id"), "/rooms/{id}");
        assert_eq!(colon_to_brace("/a/:id/b/:name"), "/a/{id}/b/{name}");
    }

    #[test]
    fn colon_to_brace_edge_cases() {
        // Empty string gets a leading slash
        assert_eq!(colon_to_brace(""), "/");
        // Root path stays as-is
        assert_eq!(colon_to_brace("/"), "/");
        // Trailing slash preserved
        assert_eq!(colon_to_brace("/rooms/"), "/rooms/");
        // Consecutive params
        assert_eq!(colon_to_brace("/:a/:b"), "/{a}/{b}");
        // No params at all
        assert_eq!(colon_to_brace("/static/css/style.css"), "/static/css/style.css");
    }

    #[test]
    fn normalize_prefix_cases() {
        // Already normalized
        assert_eq!(normalize_prefix("/api"), "/api");
        // Strips trailing slash
        assert_eq!(normalize_prefix("/api/"), "/api");
        // Adds leading slash
        assert_eq!(normalize_prefix("api"), "/api");
        // Strips trailing + adds leading
        assert_eq!(normalize_prefix("api/"), "/api");
        // Root
        assert_eq!(normalize_prefix("/"), "/");
        // Empty becomes /
        assert_eq!(normalize_prefix(""), "/");
    }

    #[test]
    fn build_matchit_path_cases() {
        // Path is root — uses prefix only
        assert_eq!(build_matchit_path("/api", "/"), "/api");
        // Path is empty — uses prefix only
        assert_eq!(build_matchit_path("/api", ""), "/api");
        // Path with leading slash — concatenated directly
        assert_eq!(build_matchit_path("/api", "/rooms"), "/api/rooms");
        // Path without leading slash — gets / separator
        assert_eq!(build_matchit_path("/api", "rooms"), "/api/rooms");
        // Params converted to braces
        assert_eq!(build_matchit_path("/api", "/rooms/:id"), "/api/rooms/{id}");
        // Nested prefix with param
        assert_eq!(build_matchit_path("/api/v1", "/:resource/:id"), "/api/v1/{resource}/{id}");
    }

    /// Helper: get a path param value as &str from resolved route path_params.
    fn get_param<'a>(params: &'a PathParams, name: &str) -> Option<&'a str> {
        for (k, v) in params {
            if *k == name {
                return std::str::from_utf8(v).ok();
            }
        }
        None
    }

    #[test]
    fn resolve_simple_get() {
        let router = MahaloRouter::new().get(
            "/health",
            plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
        );

        let resolved = router.resolve(&Method::GET, "/health");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().path_params.is_empty());
    }

    #[test]
    fn resolve_wrong_method_returns_none() {
        let router = MahaloRouter::new().get(
            "/health",
            plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
        );

        assert!(router.resolve(&Method::POST, "/health").is_none());
    }

    #[test]
    fn resolve_unknown_path_returns_none() {
        let router = MahaloRouter::new().get(
            "/health",
            plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
        );

        assert!(router.resolve(&Method::GET, "/unknown").is_none());
    }

    struct TestController;

    impl Controller for TestController {
        fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
            Box::pin(async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body("index")
            })
        }

        fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
            Box::pin(async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body("show")
            })
        }

        fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {
            Box::pin(async {
                conn.put_status(StatusCode::CREATED)
                    .put_resp_body("created")
            })
        }
    }

    #[test]
    fn resolve_scoped_resources() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Rc::new(TestController));
            });

        // GET /api/rooms -> index
        let resolved = router.resolve(&Method::GET, "/api/rooms");
        assert!(resolved.is_some());

        // GET /api/rooms/42 -> show
        let resolved = router.resolve(&Method::GET, "/api/rooms/42").unwrap();
        assert_eq!(get_param(&resolved.path_params, "id").unwrap(), "42");

        // POST /api/rooms -> create
        assert!(router.resolve(&Method::POST, "/api/rooms").is_some());

        // PUT /api/rooms/7 -> update
        let resolved = router.resolve(&Method::PUT, "/api/rooms/7").unwrap();
        assert_eq!(get_param(&resolved.path_params, "id").unwrap(), "7");

        // DELETE /api/rooms/7 -> delete
        assert!(router.resolve(&Method::DELETE, "/api/rooms/7").is_some());

        // No match
        assert!(router.resolve(&Method::PATCH, "/api/rooms/7").is_none());
    }

    #[test]
    fn resolve_with_pipelines() {
        let api_pipeline = Pipeline::new("api");

        let router = MahaloRouter::new()
            .pipeline(api_pipeline)
            .scope("/api", &["api"], |s| {
                s.get(
                    "/health",
                    plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
                );
            });

        let resolved = router.resolve(&Method::GET, "/api/health").unwrap();
        assert_eq!(resolved.pipelines.len(), 1);
        assert_eq!(resolved.pipelines[0].name, "api");
    }

    #[monoio::test(enable_timer = true)]
    async fn execute_resolved_route() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Rc::new(TestController));
            });

        let resolved = router.resolve(&Method::GET, "/api/rooms").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/rooms"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body, "index");
    }

    #[monoio::test(enable_timer = true)]
    async fn execute_with_path_params() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Rc::new(TestController));
            });

        let resolved = router.resolve(&Method::GET, "/api/rooms/99").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/rooms/99"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body, "show");
        assert_eq!(conn.path_param("id").unwrap(), "99");
    }

    #[monoio::test(enable_timer = true)]
    async fn execute_with_pipeline() {
        use mahalo_core::conn::AssignKey;

        struct AuthApplied;
        impl AssignKey for AuthApplied {
            type Value = bool;
        }

        let api_pipeline = Pipeline::new("api").plug(plug_fn(|conn: Conn| async {
            conn.assign::<AuthApplied>(true)
        }));

        let router = MahaloRouter::new()
            .pipeline(api_pipeline)
            .scope("/api", &["api"], |s| {
                s.get(
                    "/health",
                    plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
                );
            });

        let resolved = router.resolve(&Method::GET, "/api/health").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/health"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.get_assign::<AuthApplied>(), Some(&true));
    }

    #[test]
    fn router_default() {
        let router = MahaloRouter::default();
        assert!(router.resolve(&Method::GET, "/anything").is_none());
    }

    #[test]
    fn top_level_post() {
        let router = MahaloRouter::new().post(
            "/submit",
            plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }),
        );

        let resolved = router.resolve(&Method::POST, "/submit");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().path_params.is_empty());
        // GET should not match
        assert!(router.resolve(&Method::GET, "/submit").is_none());
    }

    #[test]
    fn scope_post() {
        let router = MahaloRouter::new().scope("/api", &[], |s| {
            s.post("/items", plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }));
        });
        assert!(router.resolve(&Method::POST, "/api/items").is_some());
    }

    #[test]
    fn scope_put() {
        let router = MahaloRouter::new().scope("/api", &[], |s| {
            s.put("/items/:id", plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }));
        });
        let resolved = router.resolve(&Method::PUT, "/api/items/5").unwrap();
        assert_eq!(get_param(&resolved.path_params, "id").unwrap(), "5");
    }

    #[test]
    fn scope_patch() {
        let router = MahaloRouter::new().scope("/api", &[], |s| {
            s.patch("/items/:id", plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }));
        });
        let resolved = router.resolve(&Method::PATCH, "/api/items/3").unwrap();
        assert_eq!(get_param(&resolved.path_params, "id").unwrap(), "3");
    }

    #[test]
    fn scope_delete() {
        let router = MahaloRouter::new().scope("/api", &[], |s| {
            s.delete("/items/:id", plug_fn(|conn: Conn| async { conn.put_status(StatusCode::OK) }));
        });
        assert!(router.resolve(&Method::DELETE, "/api/items/1").is_some());
    }

    #[monoio::test(enable_timer = true)]
    async fn pipeline_halt_skips_handler() {
        let auth_pipeline = Pipeline::new("auth").plug(plug_fn(|conn: Conn| async {
            conn.put_status(StatusCode::UNAUTHORIZED).halt()
        }));

        let router = MahaloRouter::new()
            .pipeline(auth_pipeline)
            .scope("/api", &["auth"], |s| {
                s.get(
                    "/secret",
                    plug_fn(|conn: Conn| async {
                        conn.put_status(StatusCode::OK)
                            .put_resp_body("secret data")
                    }),
                );
            });

        let resolved = router.resolve(&Method::GET, "/api/secret").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/secret"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::UNAUTHORIZED);
        assert!(conn.resp_body.is_empty());
    }

    #[test]
    fn path_for_with_params() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.get_named("/rooms/:id", "room_show", plug_fn(|conn: Conn| async { conn }));
            });

        let path = router.path_for("room_show", &[("id", "42")]);
        assert_eq!(path, Some("/api/rooms/42".to_string()));
    }

    #[test]
    fn path_for_unknown_name_returns_none() {
        let router = MahaloRouter::new();
        assert_eq!(router.path_for("nonexistent", &[]), None);
    }

    #[test]
    fn scoped_named_routes_include_prefix() {
        let router = MahaloRouter::new()
            .scope("/api/v1", &[], |s| {
                s.get_named("/health", "api_health", plug_fn(|conn: Conn| async { conn }));
            });

        let path = router.path_for("api_health", &[]);
        assert_eq!(path, Some("/api/v1/health".to_string()));
    }

    #[test]
    fn resources_auto_naming() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Rc::new(TestController));
            });

        assert!(router.path_for("rooms_index", &[]).is_some());
        assert_eq!(router.path_for("rooms_index", &[]), Some("/api/rooms".to_string()));
        assert_eq!(
            router.path_for("rooms_show", &[("id", "7")]),
            Some("/api/rooms/7".to_string())
        );
        assert_eq!(router.path_for("rooms_create", &[]), Some("/api/rooms".to_string()));
        assert_eq!(
            router.path_for("rooms_update", &[("id", "7")]),
            Some("/api/rooms/7".to_string())
        );
        assert_eq!(
            router.path_for("rooms_delete", &[("id", "7")]),
            Some("/api/rooms/7".to_string())
        );
    }

    #[test]
    fn named_routes_returns_expected_entries() {
        let router = MahaloRouter::new()
            .get_named("/health", "health", plug_fn(|conn: Conn| async { conn }))
            .scope("/api", &[], |s| {
                s.post_named("/login", "login", plug_fn(|conn: Conn| async { conn }));
            });

        let named = router.named_routes();
        assert_eq!(named.len(), 2);
        assert!(named.iter().any(|(n, m, p)| n == "health" && *m == Method::GET && p == "/health"));
        assert!(named.iter().any(|(n, m, p)| n == "login" && *m == Method::POST && p == "/api/login"));
    }
}
