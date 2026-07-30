use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
use anyhow::Context;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use kube::{Api, Client};
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    kube_client: Client,
    api_secret: String,
    cronjob_name: String,
    namespace: String,
}

/// Extract the bearer token or X-Api-Key from the request headers.
fn extract_token(req: &HttpRequest) -> Option<&str> {
    if let Some(auth_header) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                return Some(token);
            }
        }
    }
    if let Some(key_header) = req.headers().get("X-Api-Key") {
        if let Ok(key_str) = key_header.to_str() {
            return Some(key_str);
        }
    }
    None
}

/// Verify the request carries the correct API secret.
fn authenticate(req: &HttpRequest, api_secret: &str) -> bool {
    match extract_token(req) {
        Some(token) => token == api_secret,
        None => false,
    }
}

async fn trigger_handler(req: HttpRequest, data: web::Data<AppState>) -> HttpResponse {
    let peer = req
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    info!(
        method = %req.method(),
        path = %req.path(),
        peer = %peer,
        "Received trigger request"
    );

    if !authenticate(&req, &data.api_secret) {
        warn!(
            method = %req.method(),
            path = %req.path(),
            peer = %peer,
            "Authentication failed: invalid or missing API token"
        );
        return HttpResponse::Unauthorized().json(json!({
            "error": "Unauthorized",
            "message": "Invalid or missing API token"
        }));
    }

    info!(
        cronjob = %data.cronjob_name,
        namespace = %data.namespace,
        "Authenticated trigger request — creating job from CronJob"
    );

    match create_job_from_cronjob(&data.kube_client, &data.namespace, &data.cronjob_name).await {
        Ok(job_name) => {
            info!(job_name = %job_name, "Job created successfully");
            HttpResponse::Ok().json(json!({
                "status": "ok",
                "job": job_name
            }))
        }
        Err(e) => {
            error!(error = %e, "Failed to create job from CronJob");
            HttpResponse::InternalServerError().json(json!({
                "error": "Internal Server Error",
                "message": format!("Failed to create job: {e}")
            }))
        }
    }
}

async fn health_handler() -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn not_found_handler(req: HttpRequest) -> HttpResponse {
    let peer = req
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    warn!(
        method = %req.method(),
        path = %req.path(),
        peer = %peer,
        "Unknown URL requested"
    );

    HttpResponse::NotFound().json(json!({
        "error": "Not Found",
        "message": format!("Path '{}' not found", req.path())
    }))
}

/// Create a Kubernetes Job by instantiating the given CronJob's job template.
async fn create_job_from_cronjob(
    client: &Client,
    namespace: &str,
    cronjob_name: &str,
) -> anyhow::Result<String> {
    let cronjobs: Api<CronJob> = Api::namespaced(client.clone(), namespace);
    let cronjob = cronjobs
        .get(cronjob_name)
        .await
        .with_context(|| format!("Failed to fetch CronJob '{cronjob_name}'"))?;

    let spec = cronjob
        .spec
        .with_context(|| format!("CronJob '{cronjob_name}' has no spec"))?;

    // Build a unique job name (k8s name must be ≤ 63 chars, DNS-label safe)
    let suffix = &uuid::Uuid::new_v4().to_string().replace('-', "")[..8];
    let base = format!("{cronjob_name}-manual");
    let job_name = if base.len() + 1 + suffix.len() > 63 {
        // Truncate to fit within 63 chars
        let max_base = 63 - 1 - suffix.len();
        format!("{}-{suffix}", &base[..max_base])
    } else {
        format!("{base}-{suffix}")
    };

    let job_template = spec.job_template;
    let mut metadata = job_template.metadata.unwrap_or_default();
    metadata.name = Some(job_name.clone());
    metadata.namespace = Some(namespace.to_string());

    // Mark the job as manually instantiated (mirrors kubectl create job --from=cronjob)
    metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(
            "cronjob.kubernetes.io/instantiate".to_string(),
            "manual".to_string(),
        );

    let job = Job {
        metadata,
        spec: job_template.spec,
        ..Default::default()
    };

    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);
    let created = jobs
        .create(&Default::default(), &job)
        .await
        .with_context(|| format!("Failed to create Job '{job_name}'"))?;

    Ok(created.metadata.name.unwrap_or(job_name))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let api_secret = env::var("API_SECRET").expect("API_SECRET environment variable is required");
    if api_secret.is_empty() {
        panic!("API_SECRET must not be empty");
    }

    let cronjob_name =
        env::var("CRON_JOB_NAME").expect("CRON_JOB_NAME environment variable is required");
    let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid port number");

    info!(
        cronjob = %cronjob_name,
        namespace = %namespace,
        port = %port,
        "Starting renovate-k8s-trigger"
    );

    let kube_client = Client::try_default()
        .await
        .expect("Failed to create Kubernetes client");

    let state = web::Data::new(AppState {
        kube_client,
        api_secret,
        cronjob_name,
        namespace,
    });

    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/health", web::get().to(health_handler))
            .route("/trigger", web::get().to(trigger_handler))
            .route("/trigger", web::post().to(trigger_handler))
            .route("/trigger", web::put().to(trigger_handler))
            .default_service(web::route().to(not_found_handler))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test, web, App};

    /// Build an Authorization header value: "******"
    fn make_auth(token: &str) -> String {
        ["Bearer", " ", token].concat()
    }

    #[actix_web::test]
    async fn test_authenticate_valid_bearer() {
        let hdr = make_auth("test-token-value");
        let req = actix_web::test::TestRequest::get()
            .insert_header(("Authorization", hdr.as_str()))
            .to_http_request();
        assert!(authenticate(&req, "test-token-value"));
    }

    #[actix_web::test]
    async fn test_authenticate_valid_api_key() {
        let req = actix_web::test::TestRequest::get()
            .insert_header(("X-Api-Key", "test-token-value"))
            .to_http_request();
        assert!(authenticate(&req, "test-token-value"));
    }

    #[actix_web::test]
    async fn test_authenticate_wrong_token() {
        let hdr = make_auth("wrong-token");
        let req = actix_web::test::TestRequest::get()
            .insert_header(("Authorization", hdr.as_str()))
            .to_http_request();
        assert!(!authenticate(&req, "test-token-value"));
    }

    #[actix_web::test]
    async fn test_authenticate_missing_header() {
        let req = actix_web::test::TestRequest::get().to_http_request();
        assert!(!authenticate(&req, "test-token-value"));
    }

    #[actix_web::test]
    async fn test_authenticate_bearer_prefix_only() {
        let req = actix_web::test::TestRequest::get()
            .insert_header(("Authorization", "Bearer "))
            .to_http_request();
        assert!(!authenticate(&req, "test-token-value"));
    }

    #[actix_web::test]
    async fn test_health_endpoint() {
        let app =
            test::init_service(App::new().route("/health", web::get().to(health_handler))).await;
        let req = test::TestRequest::get().uri("/health").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_web::test]
    async fn test_not_found_endpoint() {
        let app =
            test::init_service(App::new().default_service(web::route().to(not_found_handler)))
                .await;
        let req = test::TestRequest::get().uri("/unknown-path").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
