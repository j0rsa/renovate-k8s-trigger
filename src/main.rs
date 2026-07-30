use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
use anyhow::{anyhow, Context};
use k8s_openapi::api::batch::v1::{CronJob, Job, JobSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{ListParams, ObjectMeta, PostParams};
use kube::{Api, Client};
use serde_json::json;
use std::env;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Label used to find Jobs previously created by this service for a given CronJob.
const CRONJOB_LABEL: &str = "renovate-k8s-trigger/cronjob";

#[derive(Clone)]
struct AppState {
    trigger: Arc<dyn JobTrigger>,
    api_secret: String,
    cronjob_name: String,
    namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TriggerOutcome {
    Created {
        job: String,
    },
    /// An unfinished Job for this CronJob already exists — do not create another.
    Throttled {
        job: String,
    },
}

#[async_trait::async_trait]
trait JobTrigger: Send + Sync {
    async fn trigger(&self, namespace: &str, cronjob_name: &str) -> anyhow::Result<TriggerOutcome>;
}

struct KubeJobTrigger {
    client: Client,
    /// Applied to new Jobs when the CronJob template leaves TTL unset.
    job_ttl_seconds: Option<i32>,
}

#[async_trait::async_trait]
impl JobTrigger for KubeJobTrigger {
    async fn trigger(&self, namespace: &str, cronjob_name: &str) -> anyhow::Result<TriggerOutcome> {
        create_job_from_cronjob(&self.client, namespace, cronjob_name, self.job_ttl_seconds).await
    }
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

    match data
        .trigger
        .trigger(&data.namespace, &data.cronjob_name)
        .await
    {
        Ok(TriggerOutcome::Created { job }) => {
            info!(job_name = %job, "Job created successfully");
            HttpResponse::Ok().json(json!({
                "status": "ok",
                "job": job
            }))
        }
        Ok(TriggerOutcome::Throttled { job }) => {
            info!(job_name = %job, "Trigger throttled — active job already exists");
            HttpResponse::Conflict().json(json!({
                "status": "throttled",
                "job": job,
                "message": "An active job for this CronJob already exists"
            }))
        }
        Err(e) => {
            // Log the full error chain server-side; never return internals to the client.
            error!(error = %e, error_debug = ?e, "Failed to create job from CronJob");
            HttpResponse::InternalServerError().json(json!({
                "error": "Internal Server Error",
                "message": "Failed to create job"
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

/// True when the Job has not finished (Complete/Failed).
fn job_is_active(job: &Job) -> bool {
    let Some(status) = &job.status else {
        return true;
    };
    if status.completion_time.is_some() {
        return false;
    }
    if let Some(conditions) = &status.conditions {
        for condition in conditions {
            if (condition.type_ == "Complete" || condition.type_ == "Failed")
                && condition.status == "True"
            {
                return false;
            }
        }
    }
    true
}

fn unique_job_name(cronjob_name: &str) -> String {
    let suffix = &uuid::Uuid::new_v4().to_string().replace('-', "")[..8];
    let base = format!("{cronjob_name}-manual");
    if base.len() + 1 + suffix.len() > 63 {
        let max_base = 63 - 1 - suffix.len();
        format!("{}-{suffix}", &base[..max_base])
    } else {
        format!("{base}-{suffix}")
    }
}

/// Build a Job from a CronJob template the way `kubectl create job --from=cronjob` does:
/// fresh metadata (no uid/resourceVersion/ownerRefs from the template), instantiate
/// annotation, ownerReference to the CronJob, and optional TTL for finished cleanup.
fn build_job_from_cronjob(
    cronjob: &CronJob,
    namespace: &str,
    job_name: &str,
    job_ttl_seconds: Option<i32>,
) -> anyhow::Result<Job> {
    let cronjob_name = cronjob
        .metadata
        .name
        .as_deref()
        .ok_or_else(|| anyhow!("CronJob has no name"))?;
    let cronjob_uid = cronjob
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| anyhow!("CronJob '{cronjob_name}' has no uid"))?;

    let template = cronjob
        .spec
        .as_ref()
        .and_then(|spec| spec.job_template.spec.clone())
        .ok_or_else(|| anyhow!("CronJob '{cronjob_name}' has no job template spec"))?;

    let template_meta = cronjob
        .spec
        .as_ref()
        .and_then(|spec| spec.job_template.metadata.clone())
        .unwrap_or_default();

    let mut annotations = template_meta.annotations.unwrap_or_default();
    annotations.insert(
        "cronjob.kubernetes.io/instantiate".to_string(),
        "manual".to_string(),
    );

    let mut labels = template_meta.labels.unwrap_or_default();
    labels.insert(CRONJOB_LABEL.to_string(), cronjob_name.to_string());

    let mut spec: JobSpec = template;
    if spec.ttl_seconds_after_finished.is_none() {
        if let Some(ttl) = job_ttl_seconds {
            spec.ttl_seconds_after_finished = Some(ttl);
        }
    }

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            // Match kubectl: controller=true, but do not set blockOwnerDeletion
            // (avoids needing the cronjobs/finalizer permission).
            owner_references: Some(vec![OwnerReference {
                api_version: "batch/v1".to_string(),
                kind: "CronJob".to_string(),
                name: cronjob_name.to_string(),
                uid: cronjob_uid.to_string(),
                controller: Some(true),
                block_owner_deletion: None,
            }]),
            ..Default::default()
        },
        spec: Some(spec),
        status: None,
    })
}

async fn create_job_from_cronjob(
    client: &Client,
    namespace: &str,
    cronjob_name: &str,
    job_ttl_seconds: Option<i32>,
) -> anyhow::Result<TriggerOutcome> {
    let cronjobs: Api<CronJob> = Api::namespaced(client.clone(), namespace);
    let jobs: Api<Job> = Api::namespaced(client.clone(), namespace);

    let cronjob = cronjobs
        .get(cronjob_name)
        .await
        .with_context(|| format!("Failed to fetch CronJob '{cronjob_name}'"))?;

    // Throttle: refuse to create another Job while one we previously started is still active.
    let lp = ListParams::default().labels(&format!("{CRONJOB_LABEL}={cronjob_name}"));
    let existing = jobs
        .list(&lp)
        .await
        .with_context(|| format!("Failed to list Jobs for CronJob '{cronjob_name}'"))?;

    if let Some(active) = existing.items.iter().find(|job| job_is_active(job)) {
        let name = active
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        return Ok(TriggerOutcome::Throttled { job: name });
    }

    let job_name = unique_job_name(cronjob_name);
    let job = build_job_from_cronjob(&cronjob, namespace, &job_name, job_ttl_seconds)?;

    let created = jobs
        .create(&PostParams::default(), &job)
        .await
        .with_context(|| format!("Failed to create Job '{job_name}'"))?;

    Ok(TriggerOutcome::Created {
        job: created.metadata.name.unwrap_or(job_name),
    })
}

fn parse_job_ttl(raw: Option<String>) -> Option<i32> {
    let raw = raw.filter(|s| !s.is_empty())?;
    Some(
        raw.parse::<i32>()
            .expect("JOB_TTL_SECONDS must be a non-negative integer"),
    )
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
    // Default 24h cleanup for finished manual Jobs (CronJob history limits do not apply).
    let job_ttl_seconds = parse_job_ttl(
        env::var("JOB_TTL_SECONDS")
            .ok()
            .or_else(|| Some("86400".to_string())),
    );

    info!(
        cronjob = %cronjob_name,
        namespace = %namespace,
        port = %port,
        job_ttl_seconds = ?job_ttl_seconds,
        "Starting renovate-k8s-trigger"
    );

    let kube_client = Client::try_default()
        .await
        .expect("Failed to create Kubernetes client");

    let state = web::Data::new(AppState {
        trigger: Arc::new(KubeJobTrigger {
            client: kube_client,
            job_ttl_seconds,
        }),
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
    use actix_web::{http::StatusCode, test as actix_test, web, App};
    use k8s_openapi::api::batch::v1::{CronJobSpec, JobStatus, JobTemplateSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Build an Authorization header value: "Bearer <token>"
    fn make_auth(token: &str) -> String {
        ["Bearer", " ", token].concat()
    }

    fn sample_cronjob(name: &str, uid: &str) -> CronJob {
        CronJob {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                uid: Some(uid.to_string()),
                namespace: Some("renovate-trigger".to_string()),
                ..Default::default()
            },
            spec: Some(CronJobSpec {
                job_template: JobTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(BTreeMap::from([(
                            "app".to_string(),
                            "renovate".to_string(),
                        )])),
                        annotations: Some(BTreeMap::from([("keep".to_string(), "me".to_string())])),
                        // Stale server fields that must NOT be copied onto the Job.
                        uid: Some("template-uid".to_string()),
                        resource_version: Some("12345".to_string()),
                        ..Default::default()
                    }),
                    spec: Some(JobSpec {
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            status: None,
        }
    }

    struct MockTrigger {
        outcome: Mutex<Result<TriggerOutcome, String>>,
    }

    #[async_trait::async_trait]
    impl JobTrigger for MockTrigger {
        async fn trigger(
            &self,
            _namespace: &str,
            _cronjob_name: &str,
        ) -> anyhow::Result<TriggerOutcome> {
            match &*self.outcome.lock().unwrap() {
                Ok(outcome) => Ok(outcome.clone()),
                Err(err) => Err(anyhow!(err.clone())),
            }
        }
    }

    fn test_app(
        trigger: Arc<dyn JobTrigger>,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        let state = web::Data::new(AppState {
            trigger,
            api_secret: "secret".to_string(),
            cronjob_name: "renovate".to_string(),
            namespace: "renovate-trigger".to_string(),
        });
        App::new()
            .app_data(state)
            .route("/health", web::get().to(health_handler))
            .route("/trigger", web::post().to(trigger_handler))
            .default_service(web::route().to(not_found_handler))
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
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Created {
                job: "unused".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::get().uri("/health").to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_web::test]
    async fn test_not_found_endpoint() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Created {
                job: "unused".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::get()
            .uri("/unknown-path")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn test_trigger_unauthorized_without_token() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Created {
                job: "should-not-run".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::post().uri("/trigger").to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_trigger_unauthorized_wrong_token() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Created {
                job: "should-not-run".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::post()
            .uri("/trigger")
            .insert_header(("X-Api-Key", "nope"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn test_trigger_created() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Created {
                job: "renovate-manual-abcd1234".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::post()
            .uri("/trigger")
            .insert_header(("X-Api-Key", "secret"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["job"], "renovate-manual-abcd1234");
    }

    #[actix_web::test]
    async fn test_trigger_throttled() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Ok(TriggerOutcome::Throttled {
                job: "renovate-manual-busy".into(),
            })),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::post()
            .uri("/trigger")
            .insert_header(("Authorization", make_auth("secret")))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["status"], "throttled");
        assert_eq!(body["job"], "renovate-manual-busy");
    }

    #[actix_web::test]
    async fn test_trigger_internal_error_hides_details() {
        let trigger: Arc<dyn JobTrigger> = Arc::new(MockTrigger {
            outcome: Mutex::new(Err(
                "Failed to fetch CronJob 'renovate': secrets leaked".into()
            )),
        });
        let app = actix_test::init_service(test_app(trigger)).await;
        let req = actix_test::TestRequest::post()
            .uri("/trigger")
            .insert_header(("X-Api-Key", "secret"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "Internal Server Error");
        assert_eq!(body["message"], "Failed to create job");
        let body_str = body.to_string();
        assert!(!body_str.contains("secrets leaked"));
        assert!(!body_str.contains("Failed to fetch CronJob"));
    }

    #[test]
    fn test_build_job_sanitizes_template_metadata_and_sets_owner() {
        let cronjob = sample_cronjob("renovate", "cron-uid-1");
        let job = build_job_from_cronjob(
            &cronjob,
            "renovate-trigger",
            "renovate-manual-deadbeef",
            Some(3600),
        )
        .unwrap();

        assert_eq!(
            job.metadata.name.as_deref(),
            Some("renovate-manual-deadbeef")
        );
        assert_eq!(job.metadata.namespace.as_deref(), Some("renovate-trigger"));
        assert!(job.metadata.uid.is_none());
        assert!(job.metadata.resource_version.is_none());

        let labels = job.metadata.labels.unwrap();
        assert_eq!(labels.get("app").map(String::as_str), Some("renovate"));
        assert_eq!(
            labels.get(CRONJOB_LABEL).map(String::as_str),
            Some("renovate")
        );

        let annotations = job.metadata.annotations.unwrap();
        assert_eq!(
            annotations
                .get("cronjob.kubernetes.io/instantiate")
                .map(String::as_str),
            Some("manual")
        );
        assert_eq!(annotations.get("keep").map(String::as_str), Some("me"));

        let owner = &job.metadata.owner_references.unwrap()[0];
        assert_eq!(owner.kind, "CronJob");
        assert_eq!(owner.name, "renovate");
        assert_eq!(owner.uid, "cron-uid-1");
        assert_eq!(owner.controller, Some(true));
        assert_eq!(owner.block_owner_deletion, None);

        assert_eq!(
            job.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished),
            Some(3600)
        );
    }

    #[test]
    fn test_build_job_preserves_template_ttl() {
        let mut cronjob = sample_cronjob("renovate", "cron-uid-1");
        cronjob
            .spec
            .as_mut()
            .unwrap()
            .job_template
            .spec
            .as_mut()
            .unwrap()
            .ttl_seconds_after_finished = Some(60);

        let job = build_job_from_cronjob(&cronjob, "ns", "job-1", Some(86400)).unwrap();
        assert_eq!(
            job.spec.as_ref().and_then(|s| s.ttl_seconds_after_finished),
            Some(60)
        );
    }

    #[test]
    fn test_job_is_active_heuristics() {
        let mut active = Job::default();
        assert!(job_is_active(&active));

        active.status = Some(JobStatus {
            completion_time: Some(Time(chrono::Utc::now())),
            ..Default::default()
        });
        assert!(!job_is_active(&active));

        let finished = Job {
            status: Some(JobStatus {
                conditions: Some(vec![k8s_openapi::api::batch::v1::JobCondition {
                    type_: "Complete".into(),
                    status: "True".into(),
                    last_probe_time: None,
                    last_transition_time: Some(Time(chrono::Utc::now())),
                    message: Some("done".into()),
                    reason: Some("Complete".into()),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!job_is_active(&finished));
    }
}
