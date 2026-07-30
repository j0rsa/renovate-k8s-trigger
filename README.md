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

## GitHub Actions example

The following workflow builds and pushes a Docker image, then calls `renovate-k8s-trigger` so that Renovate picks up the freshly published image immediately — no need to wait for the CronJob schedule.

Store your trigger URL and secret as [encrypted secrets](https://docs.github.com/en/actions/security-guides/encrypted-secrets) in the repository settings, e.g. `RENOVATE_TRIGGER_URL` and `RENOVATE_TRIGGER_SECRET`.

```yaml
name: Build and trigger Renovate

on:
  push:
    branches: [main]

jobs:
  docker-build:
    name: Build Docker image
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Build and push
        uses: docker/build-push-action@v5
        with:
          push: true
          tags: ghcr.io/${{ github.repository }}:latest

  trigger-renovate:
    name: Trigger Renovate
    needs: docker-build          # runs only after the image is published
    runs-on: ubuntu-latest
    steps:
      - name: Call renovate-k8s-trigger
        run: |
          curl --fail -X POST \
            -H "X-Api-Key: ${{ secrets.RENOVATE_TRIGGER_SECRET }}" \
            "${{ vars.RENOVATE_TRIGGER_URL }}/trigger"
        # Example values:
        #   RENOVATE_TRIGGER_URL  = https://renovate.example.com
        #   RENOVATE_TRIGGER_SECRET = 123ABC
```

> **Tip:** If you prefer a ****** replace the header with  
> `-H "Authorization: ****** secrets.RENOVATE_TRIGGER_SECRET }}"`.

## Docker image

Multi-arch images (`linux/amd64`, `linux/arm64`) are built and pushed to GHCR by the CI pipeline. Rust binaries are compiled **natively** on amd64 and arm64 runners (static musl), then copied into a thin `distroless/static` image — no QEMU, no in-image Rust compile.

```
ghcr.io/j0rsa/renovate-k8s-trigger:main
```

To build the image locally after producing binaries:

```bash
mkdir -p dist/amd64 dist/arm64
cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/renovate-k8s-trigger dist/amd64/
# on arm64 (or cross-compile) place the aarch64 musl binary in dist/arm64/
docker buildx build --platform linux/amd64 -t renovate-k8s-trigger:local .
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
3. **Build amd64 / arm64** — native `cargo build --release` for musl targets (parallel)
4. **Docker** — thin multi-arch image from prebuilt binaries (build-only on PRs; push to GHCR on `main`)
