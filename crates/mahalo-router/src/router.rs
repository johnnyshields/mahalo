use std::collections::HashMap;
use std::sync::Arc;

use http::Method;
use mahalo_core::conn::Conn;
use mahalo_core::controller::Controller;
use mahalo_core::pipeline::Pipeline;
use mahalo_core::plug::Plug;

/// What a matched route dispatches to.
enum Handler {
    /// A plug function (for individual route handlers).
    Plug(Box<dyn Plug>),
    /// A controller + action name (for resourceful routes).
    Controller {
        controller: Arc<dyn Controller>,
        action: String,
    },
}

/// An individual route entry.
struct Route {
    method: Method,
    /// Pattern segments, e.g. ["api", "rooms", ":id"]
    segments: Vec<String>,
    handler: Handler,
    /// Names of pipelines to run before this route's handler.
    pipeline_names: Vec<String>,
}

/// The result of resolving an incoming request to a route.
pub struct ResolvedRoute<'a> {
    pub path_params: HashMap<String, String>,
    pub pipelines: Vec<&'a Pipeline>,
    handler: &'a Handler,
}

impl<'a> ResolvedRoute<'a> {
    /// Execute the resolved route: run pipelines then the handler.
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

        // Run the handler.
        match self.handler {
            Handler::Plug(plug) => plug.call(conn).await,
            Handler::Controller { controller, action } => {
                controller.call_action(action, conn).await
            }
        }
    }
}

/// Builder used inside a `scope()` closure to register routes.
pub struct ScopeBuilder {
    prefix_segments: Vec<String>,
    pipeline_names: Vec<String>,
    routes: Vec<Route>,
}

impl ScopeBuilder {
    fn new(prefix_segments: Vec<String>, pipeline_names: Vec<String>) -> Self {
        Self {
            prefix_segments,
            pipeline_names,
            routes: Vec::new(),
        }
    }

    fn add_route(&mut self, method: Method, path: &str, handler: Handler) {
        let mut segments = self.prefix_segments.clone();
        segments.extend(parse_segments(path));
        self.routes.push(Route {
            method,
            segments,
            handler,
            pipeline_names: self.pipeline_names.clone(),
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

    /// Register standard CRUD routes for a controller:
    ///   GET    /path       -> index
    ///   GET    /path/:id   -> show
    ///   POST   /path       -> create
    ///   PUT    /path/:id   -> update
    ///   DELETE /path/:id   -> delete
    pub fn resources(&mut self, path: &str, controller: Arc<dyn Controller>) {
        let base = path;
        let with_id = format!("{}/:id", path);

        self.add_route(
            Method::GET,
            base,
            Handler::Controller {
                controller: Arc::clone(&controller),
                action: "index".to_string(),
            },
        );
        self.add_route(
            Method::GET,
            &with_id,
            Handler::Controller {
                controller: Arc::clone(&controller),
                action: "show".to_string(),
            },
        );
        self.add_route(
            Method::POST,
            base,
            Handler::Controller {
                controller: Arc::clone(&controller),
                action: "create".to_string(),
            },
        );
        self.add_route(
            Method::PUT,
            &with_id,
            Handler::Controller {
                controller: Arc::clone(&controller),
                action: "update".to_string(),
            },
        );
        self.add_route(
            Method::DELETE,
            &with_id,
            Handler::Controller {
                controller: Arc::clone(&controller),
                action: "delete".to_string(),
            },
        );
    }
}

/// Top-level router holding named pipelines and routes.
pub struct MahaloRouter {
    pipelines: HashMap<String, Pipeline>,
    routes: Vec<Route>,
}

impl MahaloRouter {
    pub fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            routes: Vec::new(),
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
        let mut builder = ScopeBuilder::new(prefix_segments, names);
        f(&mut builder);
        self.routes.extend(builder.routes);
        self
    }

    /// Add a top-level GET route (no scope, no pipelines).
    pub fn get(mut self, path: &str, plug: impl Plug) -> Self {
        self.routes.push(Route {
            method: Method::GET,
            segments: parse_segments(path),
            handler: Handler::Plug(Box::new(plug)),
            pipeline_names: Vec::new(),
        });
        self
    }

    /// Add a top-level POST route.
    pub fn post(mut self, path: &str, plug: impl Plug) -> Self {
        self.routes.push(Route {
            method: Method::POST,
            segments: parse_segments(path),
            handler: Handler::Plug(Box::new(plug)),
            pipeline_names: Vec::new(),
        });
        self
    }

    /// Resolve an incoming request method + path to a route.
    pub fn resolve(&self, method: &Method, path: &str) -> Option<ResolvedRoute<'_>> {
        let request_segments = parse_segments(path);

        for route in &self.routes {
            if &route.method != method {
                continue;
            }
            if let Some(params) = match_segments(&route.segments, &request_segments) {
                let pipelines: Vec<&Pipeline> = route
                    .pipeline_names
                    .iter()
                    .filter_map(|name| self.pipelines.get(name))
                    .collect();

                return Some(ResolvedRoute {
                    path_params: params,
                    pipelines,
                    handler: &route.handler,
                });
            }
        }
        None
    }
}

impl Default for MahaloRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a path string into non-empty segments.
fn parse_segments(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Try to match route pattern segments against request segments.
/// Returns extracted path params on success.
fn match_segments(
    pattern: &[String],
    request: &[String],
) -> Option<HashMap<String, String>> {
    if pattern.len() != request.len() {
        return None;
    }

    let mut params = HashMap::new();
    for (pat, req) in pattern.iter().zip(request.iter()) {
        if let Some(name) = pat.strip_prefix(':') {
            params.insert(name.to_string(), req.to_string());
        } else if pat != req {
            return None;
        }
    }
    Some(params)
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
    fn match_exact_path() {
        let params = match_segments(
            &parse_segments("/health"),
            &parse_segments("/health"),
        );
        assert!(params.is_some());
        assert!(params.unwrap().is_empty());
    }

    #[test]
    fn match_with_param() {
        let params = match_segments(
            &parse_segments("/rooms/:id"),
            &parse_segments("/rooms/42"),
        );
        let params = params.unwrap();
        assert_eq!(params.get("id").unwrap(), "42");
    }

    #[test]
    fn no_match_different_length() {
        let params = match_segments(
            &parse_segments("/rooms/:id"),
            &parse_segments("/rooms"),
        );
        assert!(params.is_none());
    }

    #[test]
    fn no_match_different_literal() {
        let params = match_segments(
            &parse_segments("/rooms/:id"),
            &parse_segments("/users/42"),
        );
        assert!(params.is_none());
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
                s.resources("/rooms", Arc::new(TestController));
            });

        // GET /api/rooms -> index
        let resolved = router.resolve(&Method::GET, "/api/rooms");
        assert!(resolved.is_some());

        // GET /api/rooms/42 -> show
        let resolved = router.resolve(&Method::GET, "/api/rooms/42").unwrap();
        assert_eq!(resolved.path_params.get("id").unwrap(), "42");

        // POST /api/rooms -> create
        assert!(router.resolve(&Method::POST, "/api/rooms").is_some());

        // PUT /api/rooms/7 -> update
        let resolved = router.resolve(&Method::PUT, "/api/rooms/7").unwrap();
        assert_eq!(resolved.path_params.get("id").unwrap(), "7");

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

    #[tokio::test]
    async fn execute_resolved_route() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Arc::new(TestController));
            });

        let resolved = router.resolve(&Method::GET, "/api/rooms").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/rooms"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body, "index");
    }

    #[tokio::test]
    async fn execute_with_path_params() {
        let router = MahaloRouter::new()
            .scope("/api", &[], |s| {
                s.resources("/rooms", Arc::new(TestController));
            });

        let resolved = router.resolve(&Method::GET, "/api/rooms/99").unwrap();
        let conn = Conn::new(Method::GET, Uri::from_static("/api/rooms/99"));
        let conn = resolved.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body, "show");
        assert_eq!(conn.path_params.get("id").unwrap(), "99");
    }

    #[tokio::test]
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

    #[tokio::test]
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
}
