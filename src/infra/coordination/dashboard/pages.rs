//! Dashboard pages and askama views (issue #261; see docs/coordination-design.md).

use std::sync::Arc;

use askama::Template;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;

use crate::infra::coordination::dashboard::auth::authenticate;
use crate::infra::coordination::dashboard::DashboardState;
use crate::infra::coordination::server::now_unix;

type SharedState = State<Arc<DashboardState>>;

#[derive(Template)]
#[template(path = "coordination/login.html")]
pub struct LoginTemplate {
    pub csrf: String,
}

#[derive(Template)]
#[template(path = "coordination/overview.html")]
pub struct OverviewTemplate {
    pub csrf: String,
    pub node_id: String,
    pub relay_url: String,
    pub registry_count: usize,
    pub online_count: usize,
    pub pending_count: usize,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub id: String,
    pub id_short: String,
    pub node_id_short: String,
    pub state: String,
    pub created_at: u64,
    pub age: String,
}

#[derive(Template)]
#[template(path = "coordination/requests.html")]
pub struct RequestsTemplate {
    pub csrf: String,
    pub rows: Vec<RequestRow>,
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub node_id: String,
    pub node_id_short: String,
    pub online: bool,
    pub last_seen: String,
    pub addr: String,
}

#[derive(Template)]
#[template(path = "coordination/nodes.html")]
pub struct NodesTemplate {
    pub csrf: String,
    pub rows: Vec<NodeRow>,
}

fn render<T: Template>(template: T) -> Response<Body> {
    match template.render() {
        Ok(html) => {
            let mut response = Response::new(Body::from(html));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                "text/html; charset=utf-8".parse().unwrap(),
            );
            response
        }
        Err(failure) => {
            let mut response = Response::new(Body::from(failure.to_string()));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
    }
}

fn redirect(path: &str) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response
        .headers_mut()
        .insert(header::LOCATION, path.parse().unwrap());
    response
}

fn session_or_login(
    state: &DashboardState,
    headers: &HeaderMap,
) -> Result<crate::infra::coordination::dashboard::auth::AdminSession, Box<Response<Body>>> {
    authenticate(state, headers).map_err(|_| Box::new(redirect("/dashboard/login")))
}

pub async fn login_page() -> Response<Body> {
    render(LoginTemplate {
        csrf: String::new(),
    })
}

pub async fn overview_page(
    State(state): SharedState,
    headers: HeaderMap,
) -> Result<Response<Body>, Response<Body>> {
    let session = session_or_login(&state, &headers).map_err(|boxed| *boxed)?;
    let counts = state.overview_counts(now_unix());
    Ok(render(OverviewTemplate {
        csrf: session.session.csrf,
        node_id: state.node_id.clone(),
        relay_url: state.relay_url.clone().unwrap_or_else(|| "—".to_string()),
        registry_count: counts.registry_count,
        online_count: counts.online_count,
        pending_count: counts.pending_count,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

pub async fn requests_page(
    State(state): SharedState,
    headers: HeaderMap,
) -> Result<Response<Body>, Response<Body>> {
    let session = session_or_login(&state, &headers).map_err(|boxed| *boxed)?;
    let requests =
        state.stores.requests.list().map_err(|failure| {
            plain_error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string())
        })?;
    let now = now_unix();
    let rows = requests
        .iter()
        .map(|request| RequestRow {
            id: request.id.clone(),
            id_short: request.id.chars().take(8).collect(),
            node_id_short: request.node_id.chars().take(8).collect(),
            state: request.state.as_str().to_string(),
            created_at: request.created_at,
            age: age_text(request.created_at, now),
        })
        .collect();
    Ok(render(RequestsTemplate {
        csrf: session.session.csrf,
        rows,
    }))
}

pub async fn nodes_page(
    State(state): SharedState,
    headers: HeaderMap,
) -> Result<Response<Body>, Response<Body>> {
    let session = session_or_login(&state, &headers).map_err(|boxed| *boxed)?;
    let now = now_unix();
    let nodes = state
        .registry_nodes(now)
        .map_err(|failure| plain_error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()))?;
    let rows = nodes
        .into_iter()
        .map(|node| NodeRow {
            node_id_short: node.node_id.chars().take(8).collect(),
            last_seen: node
                .last_seen
                .map(|at| age_text(at, now))
                .unwrap_or_else(|| "—".to_string()),
            addr: display_addr(&node),
            node_id: node.node_id,
            online: node.online,
        })
        .collect();
    Ok(render(NodesTemplate {
        csrf: session.session.csrf,
        rows,
    }))
}

fn display_addr(node: &crate::infra::coordination::dashboard::RegistryNode) -> String {
    let mut parts = Vec::new();
    if let Some(url) = &node.relay_url {
        parts.push(url.clone());
    }
    parts.extend(node.direct_addresses.iter().cloned());
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

fn plain_error(status: StatusCode, message: String) -> Response<Body> {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response
}

fn age_text(created_at: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(created_at);
    if elapsed < 60 {
        "<1m".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}
