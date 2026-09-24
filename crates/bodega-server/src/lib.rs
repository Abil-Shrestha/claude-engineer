//! Bodega's HTTP API: the contract every UI, bot and script builds on.
//!
//! | Method & path | What it does |
//! |---|---|
//! | `GET /api/health` | Version check |
//! | `GET /api/runs` | Run summaries, most recent first |
//! | `POST /api/runs` | Start a run (`{title, request, plan?, base_ref?, max_cost_usd?}`) |
//! | `GET /api/runs/{id}` | Full run state (tasks, attempts, checks, approvals, usage) |
//! | `GET /api/runs/{id}/events?after=&limit=` | Paged event history |
//! | `POST /api/runs/{id}/resume` | Continue an unfinished run |
//! | `GET /api/approvals` | Decisions waiting for a human, across runs |
//! | `POST /api/approvals/{id}` | Resolve one (`{decision: "approved"|"rejected", comment?, by?}`) |
//! | `GET /api/stream?after=&run=` | Server-sent events: every event after `seq`, then live |
//!
//! The stream sends each event with its `seq` as the SSE id, so a browser
//! `EventSource` resumes exactly where it left off after a disconnect
//! (`Last-Event-ID`). A client that falls behind is caught up from the log,
//! never silently dropped.
//!
//! Security: bind to localhost (the default); optionally require a bearer
//! token (`?token=` also works, since `EventSource` cannot set headers).
//! Without a token, requests must address `localhost`/a loopback IP in their
//! `Host` header, which defeats DNS-rebinding attacks from web pages.
//! Mutations only accept `application/json`, which cross-site forms cannot
//! send, and no CORS is enabled unless origins are explicitly allowed.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bodega_config::Plan;
use bodega_core::{ApprovalId, ApprovalState, Decision, Event, RunId, RunState, WorkSource};
use bodega_engine::{Engine, NewRun};
use bodega_store::{EventStore, RunSummary};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

/// How the server is exposed.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub addr: SocketAddr,
    /// Require `Authorization: Bearer <token>` (or `?token=`).
    pub token: Option<String>,
    /// Origins allowed to call the API from a browser (e.g. a UI dev server).
    pub allowed_origins: Vec<String>,
    /// A built web UI to serve at `/`.
    pub ui_dir: Option<PathBuf>,
    /// Continue unfinished runs when the server starts.
    pub resume_unfinished: bool,
}

#[derive(Clone)]
struct AppState {
    engine: Engine,
    /// Runs currently being driven by this server, so a run is never driven
    /// twice at once.
    driving: Arc<Mutex<HashSet<RunId>>>,
    token: Option<Arc<str>>,
}

impl AppState {
    fn store(&self) -> &EventStore {
        self.engine.store()
    }

    /// Drives a run in the background unless it is already being driven.
    fn spawn_drive(&self, run_id: RunId) -> bool {
        if !self.driving.lock().expect("driving lock").insert(run_id) {
            return false;
        }
        let state = self.clone();
        tokio::spawn(async move {
            if let Err(e) = state.engine.drive(run_id).await {
                tracing::error!(%run_id, "run failed to drive: {e}");
            }
            state.driving.lock().expect("driving lock").remove(&run_id);
        });
        true
    }
}

/// Builds the API router (useful for embedding and tests).
pub fn router(engine: Engine, options: &ServerOptions) -> Router {
    let state = AppState {
        engine,
        driving: Arc::default(),
        token: options.token.as_deref().map(Arc::from),
    };
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}", post(resolve_approval))
        .route("/api/stream", get(stream))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state);

    let mut app = api;
    if !options.allowed_origins.is_empty() {
        let origins: Vec<HeaderValue> = options
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        app = app.layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]),
        );
    }
    if let Some(dir) = &options.ui_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
    }
    app
}

/// Serves the API until the process is stopped.
pub async fn serve(engine: Engine, options: ServerOptions) -> std::io::Result<()> {
    let app = router(engine.clone(), &options);
    if options.resume_unfinished {
        let runs = engine
            .store()
            .list_runs()
            .await
            .map_err(std::io::Error::other)?;
        let state = AppState {
            engine,
            driving: Arc::default(),
            token: None,
        };
        for run in runs.iter().filter(|r| !r.status.is_terminal()) {
            tracing::info!(run = %run.run_id, "resuming unfinished run");
            state.spawn_drive(run.run_id);
        }
    }
    let listener = tokio::net::TcpListener::bind(options.addr).await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await
}

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(expected) = &state.token else {
        // Without a token the API trusts local callers, so make sure the
        // caller really addressed this machine: a page on another domain that
        // rebinds its DNS to 127.0.0.1 still sends its own name as `Host`.
        if !is_local_host(request.headers()) {
            return ApiError(
                StatusCode::FORBIDDEN,
                "requests must address localhost (or run the server with --token)".into(),
            )
            .into_response();
        }
        return next.run(request).await;
    };
    let from_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let from_query = request
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")));
    let presented = from_header.or(from_query).unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "missing or invalid token".into()).into_response()
    }
}

/// Whether the `Host` header names the loopback interface.
fn is_local_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: `[::1]:7777`.
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// An API error rendered as `{"error": "..."}`.
#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<bodega_store::StoreError> for ApiError {
    fn from(e: bodega_store::StoreError) -> Self {
        match e {
            bodega_store::StoreError::Rejected(e) => ApiError(StatusCode::CONFLICT, e.to_string()),
            other => ApiError(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        }
    }
}

impl From<bodega_engine::EngineError> for ApiError {
    fn from(e: bodega_engine::EngineError) -> Self {
        use bodega_engine::EngineError as E;
        let status = match &e {
            E::Plan(_) | E::UnknownRuntime(..) | E::Git(_) => StatusCode::BAD_REQUEST,
            E::UnknownRun(_) => StatusCode::NOT_FOUND,
            E::Store(bodega_store::StoreError::Rejected(_)) => StatusCode::CONFLICT,
            E::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn parse_run_id(raw: &str) -> ApiResult<RunId> {
    raw.parse()
        .map_err(|e: bodega_core::ParseIdError| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

async fn load_run(state: &AppState, run_id: RunId) -> ApiResult<RunState> {
    state
        .store()
        .run(run_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("run {run_id} not found")))
}

#[derive(Serialize)]
struct Health {
    name: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        name: "bodega",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn list_runs(State(state): State<AppState>) -> ApiResult<Json<Vec<RunSummary>>> {
    Ok(Json(state.store().list_runs().await?))
}

/// Body of `POST /api/runs`.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct CreateRun {
    pub title: String,
    #[serde(default)]
    #[ts(optional)]
    pub request: Option<String>,
    /// Without a plan, the whole request is one task.
    #[serde(default)]
    #[ts(optional)]
    pub plan: Option<Plan>,
    #[serde(default)]
    #[ts(optional)]
    pub base_ref: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, ts_rs::TS)]
pub struct Created {
    pub run_id: RunId,
}

async fn create_run(
    State(state): State<AppState>,
    Json(body): Json<CreateRun>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    if body.title.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "title is required".into(),
        ));
    }
    let request = body.request.unwrap_or_else(|| body.title.clone());
    let plan = body
        .plan
        .unwrap_or_else(|| Plan::single(&body.title, &request));
    let run_id = state
        .engine
        .create_run(NewRun {
            title: body.title,
            request,
            source: WorkSource::Manual,
            base_ref: body.base_ref,
            plan,
            max_cost_usd: body.max_cost_usd,
            max_tokens: None,
        })
        .await?;
    state.spawn_drive(run_id);
    Ok((StatusCode::CREATED, Json(Created { run_id })))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunState>> {
    let run_id = parse_run_id(&id)?;
    Ok(Json(load_run(&state, run_id).await?))
}

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    500
}

async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
) -> ApiResult<Json<Vec<Event>>> {
    let run_id = parse_run_id(&id)?;
    load_run(&state, run_id).await?;
    let limit = page.limit.clamp(1, 5_000);
    Ok(Json(
        state.store().run_events(run_id, page.after, limit).await?,
    ))
}

async fn resume_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let run_id = parse_run_id(&id)?;
    let run = load_run(&state, run_id).await?;
    if run.status.is_terminal() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("run {run_id} already finished"),
        ));
    }
    state.spawn_drive(run_id);
    Ok(StatusCode::ACCEPTED)
}

/// A pending approval with the run it belongs to.
#[derive(Debug, Serialize, Deserialize, ts_rs::TS)]
pub struct PendingApproval {
    pub run_id: RunId,
    pub run_title: String,
    #[serde(flatten)]
    pub approval: ApprovalState,
}

async fn list_approvals(State(state): State<AppState>) -> ApiResult<Json<Vec<PendingApproval>>> {
    let mut out = Vec::new();
    for summary in state.store().list_runs().await? {
        if summary.pending_approvals == 0 {
            continue;
        }
        let run = load_run(&state, summary.run_id).await?;
        out.extend(run.pending_approvals().map(|a| PendingApproval {
            run_id: run.id,
            run_title: run.spec.title.clone(),
            approval: a.clone(),
        }));
    }
    Ok(Json(out))
}

/// Body of `POST /api/approvals/{id}`.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(deny_unknown_fields)]
pub struct Resolve {
    pub decision: Decision,
    #[serde(default)]
    #[ts(optional)]
    pub comment: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub by: Option<String>,
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Resolve>,
) -> ApiResult<StatusCode> {
    let approval_id: ApprovalId = id
        .parse()
        .map_err(|e: bodega_core::ParseIdError| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    for summary in state.store().list_runs().await? {
        let run = load_run(&state, summary.run_id).await?;
        if run.approvals.contains_key(&approval_id) {
            state
                .engine
                .resolve_approval(
                    run.id,
                    approval_id,
                    body.decision,
                    body.by.as_deref().unwrap_or("api"),
                    body.comment,
                )
                .await?;
            return Ok(StatusCode::NO_CONTENT);
        }
    }
    Err(ApiError(
        StatusCode::NOT_FOUND,
        format!("approval {approval_id} not found"),
    ))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    run: Option<String>,
}

async fn stream(
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    // A reconnecting EventSource sends the last id it saw.
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let after = last_event_id.or(query.after).unwrap_or(0);
    let run = query.run.as_deref().map(parse_run_id).transpose()?;
    Ok(Sse::new(event_stream(state.store().clone(), after, run)).keep_alive(KeepAlive::default()))
}

/// Every event with `seq > after` (optionally for one run): the backlog from
/// the log first, then live events. Gaps (a lagging subscriber, or events
/// written by another process) are filled from the log, which is also polled
/// periodically.
fn event_stream(
    store: EventStore,
    mut after: u64,
    run: Option<RunId>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    async_stream::stream! {
        let mut live = store.subscribe();
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut catch_up = true;
        loop {
            if catch_up {
                loop {
                    let batch = match store.events_after(after, 1_000).await {
                        Ok(batch) => batch,
                        Err(e) => {
                            yield Ok(SseEvent::default().event("error").data(e.to_string()));
                            return;
                        }
                    };
                    if batch.is_empty() {
                        break;
                    }
                    for event in batch {
                        after = event.seq;
                        if run.is_none_or(|r| r == event.run_id) {
                            yield Ok(to_sse(&event));
                        }
                    }
                }
                catch_up = false;
            }
            tokio::select! {
                received = live.recv() => match received {
                    Ok(event) if event.seq <= after => {}
                    Ok(event) if event.seq == after + 1 => {
                        after = event.seq;
                        if run.is_none_or(|r| r == event.run_id) {
                            yield Ok(to_sse(&event));
                        }
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => catch_up = true,
                    Err(RecvError::Closed) => return,
                },
                _ = poll.tick() => catch_up = true,
            }
        }
    }
}

fn to_sse(event: &Event) -> SseEvent {
    SseEvent::default()
        .id(event.seq.to_string())
        .json_data(event)
        .unwrap_or_else(|e| SseEvent::default().event("error").data(e.to_string()))
}
