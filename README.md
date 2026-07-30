# renovate-k8s-trigger

**On-demand trigger for a self-hosted [Renovate](https://docs.renovatebot.com/) CronJob in Kubernetes.**

Self-hosted Renovate is often deployed as a Kubernetes `CronJob` — reliable, but it only runs on a schedule. When your CI publishes a new image on `main`, you usually want Renovate to pick it up *now*, not an hour later.

This small Rust API sits next to that CronJob. After a successful build and push, GitHub Actions calls `/trigger`. The service instantiates a Job from the existing Renovate CronJob template — the same idea as:

```bash
kubectl create job --from=cronjob/renovate renovate-manual-$(date +%s)
```

…but callable over HTTPS with a shared secret, from CI, without cluster credentials in the workflow.

```
  GitHub Actions (main)
        │
        │  1. build & push image → GHCR
        │  2. POST /trigger  (X-Api-Key)
        ▼
  renovate-k8s-trigger  ──creates Job──▶  Renovate CronJob template
                                                    │
                                                    ▼
                                            Renovate runs once,
                                            opens/updates PRs
```

---

## Why this exists

| Without this | With this |
|---|---|
| Renovate wakes up on a cron schedule | Renovate runs as soon as a new image lands |
| CI finishes; registry is updated; bots wait | CI finishes → trigger → rollout PRs start |
| Manual `kubectl create job --from=…` | One authenticated HTTP call from Actions |

It is intentionally narrow: authenticate the caller, create one Job from a named CronJob, throttle if Renovate is already running. Not a general webhook router — just the missing “run Renovate now” button for a cluster-hosted bot.

---

## Quick start

```bash
# Namespace + RBAC
kubectl create namespace renovate-trigger
kubectl apply -f k8s/rbac.yaml

# Shared secret (must match what GitHub Actions will send)
kubectl create secret generic renovate-k8s-trigger-secret \
  --namespace renovate-trigger \
  --from-literal=api-secret="$(openssl rand -hex 32)"

# Point CRON_JOB_NAME at your Renovate CronJob, then deploy
# (edit k8s/deployment.yaml — remove or fix the sample Secret block if you created the secret above)
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml

# Optional: public HTTPS endpoint for Actions
# kubectl apply -f k8s/ingress.yaml
```

Smoke test:

```bash
curl -sS -X POST \
  -H "X-Api-Key: $API_SECRET" \
  https://renovate-trigger.example.com/trigger
# {"status":"ok","job":"renovate-manual-a3f9c12b"}
```

---

## API

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` / `POST` / `PUT` | `/trigger` | required | Create a Job from the configured CronJob |
| `GET` | `/health` | none | Liveness / readiness |

### Successful trigger

```http
HTTP/1.1 200 OK
{"status":"ok","job":"renovate-manual-a3f9c12b"}
```

### Already running (throttled)

If a previous manual Job for this CronJob is still active, a new one is **not** started:

```http
HTTP/1.1 409 Conflict
{"status":"throttled","job":"renovate-manual-busy","message":"An active job for this CronJob already exists"}
```

### Auth failure / unknown path

`401` for missing/invalid tokens (logged). `404` for unknown URLs (logged). Internal failures return a generic `500` — details stay in server logs only.

---

## Authentication

Send the shared secret with either header:

```bash
# X-Api-Key
curl -H "X-Api-Key: $API_SECRET" -X POST https://trigger.example.com/trigger

# Bearer token
curl -H "Authorization: Bearer $API_SECRET" -X POST https://trigger.example.com/trigger
```

---

## GitHub Actions — trigger after the image is published

Call the API only **after** the image is in the registry. Store the URL as a variable and the secret as an encrypted secret (e.g. `RENOVATE_TRIGGER_URL`, `RENOVATE_TRIGGER_SECRET`).

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
    needs: docker-build
    runs-on: ubuntu-latest
    steps:
      - name: Call renovate-k8s-trigger
        run: |
          curl --fail -X POST \
            -H "X-Api-Key: ${{ secrets.RENOVATE_TRIGGER_SECRET }}" \
            "${{ vars.RENOVATE_TRIGGER_URL }}/trigger"
```

---

## Configuration

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `API_SECRET` | yes | — | Shared token for `/trigger` |
| `CRON_JOB_NAME` | yes | — | Renovate CronJob to instantiate |
| `NAMESPACE` | no | `default` | Namespace of the CronJob (Downward API in the sample manifests) |
| `PORT` | no | `8080` | Listen port |
| `JOB_TTL_SECONDS` | no | `86400` | TTL after finish for new Jobs when the CronJob template omits one; set empty to disable |
| `RUST_LOG` | no | `info` | Log filter |

The CronJob and this trigger should live in the **same namespace** when using the sample manifests (`NAMESPACE` is injected from the pod).

Jobs are built like `kubectl create job --from=cronjob/…`: fresh metadata, `cronjob.kubernetes.io/instantiate=manual`, owner reference to the CronJob. Finished manual Jobs are not cleaned by CronJob history limits — hence the default TTL.

---

## Container image

Multi-arch (`linux/amd64`, `linux/arm64`) images are published to GHCR on pushes to `main`:

```text
ghcr.io/j0rsa/renovate-k8s-trigger:main
```

CI compiles static musl binaries on native amd64 and arm64 runners, then packs a thin `distroless` image (no QEMU). Pull requests build the image to verify the pipeline but do not push.

---

## Local development

Needs a reachable Kubernetes API (kubeconfig / in-cluster config) and a CronJob named by `CRON_JOB_NAME`.

```bash
export API_SECRET=dev-secret
export CRON_JOB_NAME=renovate
export NAMESPACE=default
cargo run
```

```bash
cargo test
cargo clippy -- -D warnings
```

---

## Repository layout

```text
src/main.rs          # HTTP API + Job creation / throttle logic
k8s/                 # RBAC, Deployment, Service, Ingress examples
Dockerfile           # distroless image from prebuilt binaries
.github/workflows/   # lint, test, native multi-arch build, image publish
```

## License

Use and adapt as needed for your cluster. Contributions welcome via pull request.
