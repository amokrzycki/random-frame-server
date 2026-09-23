use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;

const DEFAULT_MAX_PAYLOAD: usize = 16_000_068;
type ApiResult = Result<Response, StatusCode>;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let bind: SocketAddr = env::var("RF_SYNC_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()?;
    let db_path = PathBuf::from(env::var("RF_SYNC_DB").unwrap_or_else(|_| "sync.db".into()));
    let max_payload: usize = env::var("RF_SYNC_MAX_PAYLOAD")
        .unwrap_or_else(|_| DEFAULT_MAX_PAYLOAD.to_string())
        .parse()?;
    if !(1..=DEFAULT_MAX_PAYLOAD).contains(&max_payload) {
        return Err("RF_SYNC_MAX_PAYLOAD must be 1..=16000068".into());
    }
    let db = open_db(&db_path).await?;
    let app = app(AppState { db }, max_payload);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "a missing signal handler prevents graceful shutdown"
)]
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.expect("Ctrl+C handler");
}

async fn open_db(path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5));
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    sqlx::query(include_str!("../migrations/001_initial.sql"))
        .execute(&db)
        .await?;
    Ok(db)
}

fn app(state: AppState, max_payload: usize) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sync/{sync_id}", get(read).put(write))
        .layer(DefaultBodyLimit::max(max_payload))
        .layer(TraceLayer::new_for_http()
            .make_span_with(|request: &axum::http::Request<_>| {
                let route = request.extensions().get::<axum::extract::MatchedPath>()
                    .map_or("unknown", axum::extract::MatchedPath::as_str);
                tracing::info_span!("request", method = %request.method(), route)
            })
            .on_response(|response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
                tracing::info!(status = %response.status(), latency_ms = latency.as_millis(), "response");
            }))
        .with_state(state)
}

fn valid_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn bearer(headers: &HeaderMap) -> Result<[u8; 32], StatusCode> {
    let value = only_header(headers, header::AUTHORIZATION)?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::BAD_REQUEST)?;
    if !valid_hex64(token) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Sha256::digest(token.as_bytes()).into())
}

fn only_header(headers: &HeaderMap, name: header::HeaderName) -> Result<&str, StatusCode> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(StatusCode::BAD_REQUEST)?;
    if values.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    value.to_str().map_err(|_| StatusCode::BAD_REQUEST)
}

fn content_type(headers: &HeaderMap) -> Result<(), StatusCode> {
    if only_header(headers, header::CONTENT_TYPE)? == "application/octet-stream" {
        Ok(())
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

fn revision(headers: &HeaderMap) -> Result<Option<i64>, StatusCode> {
    if headers.contains_key(header::IF_NONE_MATCH) && headers.contains_key(header::IF_MATCH) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if headers.contains_key(header::IF_NONE_MATCH) {
        return if only_header(headers, header::IF_NONE_MATCH)? == "*" {
            Ok(None)
        } else {
            Err(StatusCode::BAD_REQUEST)
        };
    }
    let value = only_header(headers, header::IF_MATCH)?;
    let digits = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or(StatusCode::BAD_REQUEST)?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let number = digits.parse::<i64>().map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Some(number))
}

fn validate_id(sync_id: &str) -> Result<(), StatusCode> {
    if valid_hex64(sync_id) {
        Ok(())
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a Result::map_err callback"
)]
fn db_error(error: sqlx::Error) -> StatusCode {
    let kind = match &error {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => "unavailable",
        sqlx::Error::Database(db) => match db
            .code()
            .and_then(|code| code.parse::<i32>().ok())
            .map(|code| code & 0xff)
        {
            Some(5 | 6 | 10 | 14) => "unavailable",
            Some(11 | 26) => "corrupt",
            _ => "internal",
        },
        _ => "internal",
    };
    tracing::error!(kind, "storage failure");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn health(State(state): State<AppState>) -> ApiResult {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .map_err(db_error)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        "{\"status\":\"ok\"}",
    )
        .into_response())
}

async fn read(
    State(state): State<AppState>,
    Path(sync_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    validate_id(&sync_id)?;
    let verifier = bearer(&headers)?;
    let row =
        sqlx::query("SELECT auth_verifier, revision, payload FROM sync_chains WHERE sync_id = ?")
            .bind(sync_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;
    let Some(row) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let stored: Vec<u8> = row.try_get("auth_verifier").map_err(db_error)?;
    if !bool::from(stored.ct_eq(&verifier)) {
        return Err(StatusCode::NOT_FOUND);
    }
    let revision: i64 = row.try_get("revision").map_err(db_error)?;
    let payload: Vec<u8> = row.try_get("payload").map_err(db_error)?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::ETAG, etag(revision)),
        ],
        payload,
    )
        .into_response())
}

async fn write(
    State(state): State<AppState>,
    Path(sync_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    validate_id(&sync_id)?;
    let verifier = bearer(&headers)?;
    content_type(&headers)?;
    let expected = revision(&headers)?;
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let now = unix_time()?;
    match expected {
        None => {
            let result = sqlx::query("INSERT INTO sync_chains (sync_id, auth_verifier, revision, payload, created_at, updated_at) VALUES (?, ?, 1, ?, ?, ?) ON CONFLICT(sync_id) DO NOTHING")
                .bind(sync_id).bind(verifier.as_slice()).bind(body.as_ref()).bind(now).bind(now)
                .execute(&state.db).await.map_err(db_error)?;
            if result.rows_affected() == 0 {
                return Err(StatusCode::PRECONDITION_FAILED);
            }
            Ok((StatusCode::CREATED, [(header::ETAG, etag(1))]).into_response())
        }
        Some(expected) => {
            let stored: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT auth_verifier FROM sync_chains WHERE sync_id = ?")
                    .bind(&sync_id)
                    .fetch_optional(&state.db)
                    .await
                    .map_err(db_error)?;
            let Some(stored) = stored else {
                return Err(StatusCode::NOT_FOUND);
            };
            if !bool::from(stored.ct_eq(&verifier)) {
                return Err(StatusCode::NOT_FOUND);
            }
            let changed: Option<i64> = sqlx::query_scalar("UPDATE sync_chains SET payload = ?, revision = revision + 1, updated_at = ? WHERE sync_id = ? AND revision = ? AND revision < 9223372036854775807 RETURNING revision")
                .bind(body.as_ref()).bind(now).bind(&sync_id).bind(expected)
                .fetch_optional(&state.db).await.map_err(db_error)?;
            match changed {
                Some(next) => {
                    Ok((StatusCode::NO_CONTENT, [(header::ETAG, etag(next))]).into_response())
                }
                None if expected == i64::MAX => {
                    let actual: Option<i64> =
                        sqlx::query_scalar("SELECT revision FROM sync_chains WHERE sync_id = ?")
                            .bind(sync_id)
                            .fetch_optional(&state.db)
                            .await
                            .map_err(db_error)?;
                    if actual == Some(i64::MAX) {
                        tracing::error!(kind = "revision_overflow", "storage failure");
                        Err(StatusCode::INTERNAL_SERVER_ERROR)
                    } else {
                        Err(StatusCode::PRECONDITION_FAILED)
                    }
                }
                None => Err(StatusCode::PRECONDITION_FAILED),
            }
        }
    }
}

fn unix_time() -> Result<i64, StatusCode> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[expect(
    clippy::expect_used,
    reason = "a quoted decimal integer is always a valid header value"
)]
fn etag(revision: i64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).expect("valid revision ETag")
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test setup and assertions fail immediately on unexpected errors"
)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    const TOKEN: &str = "849d889a5511423d18dffe61c92796005318fa916449dcc3e6fe0e9d8d8b9d90";
    const WRONG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    async fn setup(max: usize) -> (TempDir, AppState, Router) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            db: open_db(&dir.path().join("sync.db")).await.unwrap(),
        };
        let router = app(state.clone(), max);
        (dir, state, router)
    }

    async fn request(
        router: &Router,
        method: Method,
        id: &str,
        token: Option<&str>,
        conditional: Option<(&str, &str)>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(format!("/sync/{id}"));
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some((name, value)) = conditional {
            builder = builder.header(name, value);
        }
        if let Some(value) = content_type {
            builder = builder.header(header::CONTENT_TYPE, value);
        }
        router
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }

    async fn put(
        router: &Router,
        token: &str,
        conditional: (&str, &str),
        body: Vec<u8>,
    ) -> Response {
        request(
            router,
            Method::PUT,
            ID,
            Some(token),
            Some(conditional),
            Some("application/octet-stream"),
            body,
        )
        .await
    }

    fn tag(response: &Response) -> &str {
        response.headers()[header::ETAG].to_str().unwrap()
    }

    #[test]
    fn client_auth_vector() {
        assert_eq!(
            format!("{:x}", Sha256::digest(TOKEN.as_bytes())),
            "b4029f45e1ecb8ff49a25ab1f5ab5527620de4beda89d948c9b939d19c02804c"
        );
    }

    #[tokio::test]
    async fn http_create_get_update_and_privacy() {
        let (_dir, _state, router) = setup(100).await;
        let health = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(health.into_body(), 100).await.unwrap(),
            "{\"status\":\"ok\"}"
        );
        let original = vec![0, 255, 0x80, b'R', 0];
        let created = put(&router, TOKEN, ("if-none-match", "*"), original.clone()).await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert_eq!(tag(&created), "\"1\"");
        assert_eq!(
            put(&router, TOKEN, ("if-none-match", "*"), vec![1])
                .await
                .status(),
            StatusCode::PRECONDITION_FAILED
        );

        let got = request(&router, Method::GET, ID, Some(TOKEN), None, None, vec![]).await;
        assert_eq!(got.status(), StatusCode::OK);
        assert_eq!(tag(&got), "\"1\"");
        assert_eq!(
            got.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(to_bytes(got.into_body(), 100).await.unwrap(), original);
        for (id, token) in [(ID, WRONG), (WRONG, TOKEN)] {
            let response = request(&router, Method::GET, id, Some(token), None, None, vec![]).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::ETAG));
            assert!(
                to_bytes(response.into_body(), 100)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        let next = vec![255, 0, 1];
        let updated = put(&router, TOKEN, ("if-match", "\"1\""), next.clone()).await;
        assert_eq!(updated.status(), StatusCode::NO_CONTENT);
        assert_eq!(tag(&updated), "\"2\"");
        for rev in ["\"1\"", "\"3\""] {
            assert_eq!(
                put(&router, TOKEN, ("if-match", rev), vec![9])
                    .await
                    .status(),
                StatusCode::PRECONDITION_FAILED
            );
            assert_eq!(
                put(&router, WRONG, ("if-match", rev), vec![9])
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
        let got = request(&router, Method::GET, ID, Some(TOKEN), None, None, vec![]).await;
        assert_eq!(tag(&got), "\"2\"");
        assert_eq!(to_bytes(got.into_body(), 100).await.unwrap(), next);
    }

    #[tokio::test]
    async fn http_validation() {
        let (_dir, _state, router) = setup(4).await;
        let valid = || vec![1];
        let create = Some(("if-none-match", "*"));
        let octet = Some("application/octet-stream");
        for (id, token, cond, ct, body) in [
            ("bad", Some(TOKEN), create, octet, valid()),
            (ID, None, create, octet, valid()),
            (ID, Some("ABC"), create, octet, valid()),
            (ID, Some(TOKEN), None, octet, valid()),
            (
                ID,
                Some(TOKEN),
                Some(("if-none-match", "x")),
                octet,
                valid(),
            ),
            (ID, Some(TOKEN), create, None, valid()),
            (ID, Some(TOKEN), create, Some("application/json"), valid()),
            (ID, Some(TOKEN), create, octet, vec![]),
        ] {
            assert_eq!(
                request(&router, Method::PUT, id, token, cond, ct, body)
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            put(&router, TOKEN, ("if-none-match", "*"), vec![1; 5])
                .await
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            put(&router, TOKEN, ("if-none-match", "*"), vec![0, 255, 0, 128])
                .await
                .status(),
            StatusCode::CREATED
        );
        for bad in [
            "1",
            "\"0\"",
            "\"01\"",
            "W/\"1\"",
            "\"1\",\"2\"",
            "*",
            "\"9223372036854775808\"",
        ] {
            assert_eq!(
                put(&router, TOKEN, ("if-match", bad), valid())
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            request(&router, Method::GET, "BAD", Some(TOKEN), None, None, vec![])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(&router, Method::GET, ID, Some("A"), None, None, vec![])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(&router, Method::GET, ID, None, None, None, vec![])
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn concurrent_cas_and_restart() {
        let (dir, state, router) = setup(100).await;
        assert_eq!(
            put(&router, TOKEN, ("if-none-match", "*"), vec![0])
                .await
                .status(),
            StatusCode::CREATED
        );
        for rev in 1_u8..10 {
            assert_eq!(
                put(
                    &router,
                    TOKEN,
                    ("if-match", &format!("\"{rev}\"")),
                    vec![rev]
                )
                .await
                .status(),
                StatusCode::NO_CONTENT
            );
        }
        let (a, b) = tokio::join!(
            put(&router, TOKEN, ("if-match", "\"10\""), vec![11]),
            put(&router, TOKEN, ("if-match", "\"10\""), vec![22]),
        );
        let statuses = [a.status(), b.status()];
        assert!(statuses.contains(&StatusCode::NO_CONTENT));
        assert!(statuses.contains(&StatusCode::PRECONDITION_FAILED));
        drop(router);
        state.db.close().await;
        let reopened = open_db(&dir.path().join("sync.db")).await.unwrap();
        let router = app(AppState { db: reopened }, 100);
        let got = request(&router, Method::GET, ID, Some(TOKEN), None, None, vec![]).await;
        assert_eq!(tag(&got), "\"11\"");
        assert!(matches!(
            to_bytes(got.into_body(), 100).await.unwrap().as_ref(),
            [11 | 22]
        ));
        assert_eq!(
            request(&router, Method::GET, ID, Some(WRONG), None, None, vec![])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn concurrent_create_has_one_winner() {
        let (_dir, _state, router) = setup(100).await;
        let (a, b) = tokio::join!(
            put(&router, TOKEN, ("if-none-match", "*"), vec![1]),
            put(&router, TOKEN, ("if-none-match", "*"), vec![2]),
        );
        let statuses = [a.status(), b.status()];
        assert!(statuses.contains(&StatusCode::CREATED));
        assert!(statuses.contains(&StatusCode::PRECONDITION_FAILED));
    }

    #[tokio::test]
    async fn revision_overflow_is_controlled() {
        let (_dir, state, router) = setup(100).await;
        put(&router, TOKEN, ("if-none-match", "*"), vec![1]).await;
        sqlx::query("UPDATE sync_chains SET revision = ? WHERE sync_id = ?")
            .bind(i64::MAX)
            .bind(ID)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            put(
                &router,
                TOKEN,
                ("if-match", &format!("\"{}\"", i64::MAX)),
                vec![2]
            )
            .await
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let got = request(&router, Method::GET, ID, Some(TOKEN), None, None, vec![]).await;
        assert_eq!(tag(&got), format!("\"{}\"", i64::MAX));
        assert_eq!(to_bytes(got.into_body(), 100).await.unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn protocol_maximum() {
        let (_dir, _state, router) = setup(DEFAULT_MAX_PAYLOAD).await;
        assert_eq!(
            put(
                &router,
                TOKEN,
                ("if-none-match", "*"),
                vec![0; DEFAULT_MAX_PAYLOAD]
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            put(
                &router,
                TOKEN,
                ("if-match", "\"1\""),
                vec![0; DEFAULT_MAX_PAYLOAD + 1]
            )
            .await
            .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
