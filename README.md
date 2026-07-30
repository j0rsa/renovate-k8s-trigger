# renovate-k8s-trigger

A small [actix-web](https://actix.rs/) Rust API server that triggers a Kubernetes Job from an existing CronJob.  
Designed to be called from GitHub Webhooks or GitHub Actions.

## Endpoints

| Method | Path      | Description                    |
|--------|-----------|--------------------------------|
| GET    | /trigger  | Trigger the configured CronJob |
| POST   | /trigger  | Trigger the configured CronJob |
| PUT    | /trigger  | Trigger the configured CronJob |
| GET    | /health   | Liveness / readiness probe     |

All `/trigger` requests require an API token; unknown paths return `404`.

## Authentication

Pass the secret as a ******** or an **`X-Api-Key`** header:

```bash
# ****** -H "Authorization: ******" https://trigger.example.com/trigger

# X-Api-Key
curl -H "X-Api-Key: <API_SECRET>" https://trigger.example.com/trigger
```

## Environment Variables

| Variable       | Required | Default   | Description                          |
|----------------|----------|-----------|--------------------------------------|
| `API_SECRET`   | ✅       | —         | Secret token for authentication      |
| `CRON_JOB_NAME`| ✅       | —         | Name of the CronJob to instantiate   |
| `NAMESPACE`    | ❌       | `default` | Kubernetes namespace (auto-injected via Downward API in the k8s manifests) |
| `PORT`         | ❌       | `8080`    | HTTP listen port                     |
| `RUST_LOG`     | ❌       | `info`    | Log level filter                     |

## Kubernetes deployment

```bash
# 1. Create the namespace
kubectl create namespace renovate-trigger

# 2. Apply RBAC
kubectl apply -f k8s/rbac.yaml

# 3. Create the API secret
kubectl create secret generic renovate-k8s-trigger-secret \
  --namespace renovate-trigger \
  --from-literal=api-secret="$(openssl rand -hex 32)"

# 4. Apply the deployment and service
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml

# 5. (optional) Expose via Ingress — edit k8s/ingress.yaml first
kubectl apply -f k8s/ingress.yaml
```

## Docker image

Multi-arch images (`linux/amd64`, `linux/arm64`) are built and pushed to GHCR by the CI pipeline on every push to `main`:

```
ghcr.io/j0rsa/renovate-k8s-trigger:main
```

## Local development

```bash
export API_SECRET=dev-secret
export CRON_JOB_NAME=renovate
export NAMESPACE=default
cargo run
```

## CI

The GitHub Actions workflow (`.github/workflows/ci.yml`) runs:

1. **Lint** — `cargo fmt --check` + `cargo clippy`
2. **Test** — `cargo test`
3. **Docker** — multi-arch build (`linux/amd64`, `linux/arm64`) and push to GHCR (on `main` branch only)
