# renovate-k8s-trigger

A lightweight Rust/actix-web API server that bridges **GitHub webhooks or Actions** with [OSS Renovate](https://github.com/renovatebot/renovate) running as a Kubernetes CronJob.  
When a push, pull-request, or any other event arrives, the API instantly creates a one-off Kubernetes Job from the existing CronJob so Renovate runs immediately—without waiting for its next scheduled execution.

---

## Use case

OSS Renovate is commonly self-hosted in Kubernetes as a CronJob that runs on a schedule (e.g. every few hours or once a day).  
This works well for routine dependency updates, but introduces latency: if a new release drops right after the scheduled run, the next Renovate scan might not happen for hours.

`renovate-k8s-trigger` solves this by exposing a simple HTTP endpoint that can be called from:

- **GitHub webhooks** – fire a trigger every time a repository event (push, release, PR merge, …) occurs.
- **GitHub Actions** – add a step to any workflow that calls the endpoint after a relevant action.

```
GitHub event
     │
     ▼
renovate-k8s-trigger  ──► Kubernetes API  ──► new Job (copy of CronJob spec)
                                                    │
                                                    ▼
                                              Renovate scans repos & opens PRs
```

The service runs inside the same Kubernetes cluster as Renovate and uses a dedicated ServiceAccount with the minimum RBAC permissions needed to create Jobs from the CronJob.

---

## Features

- **GET / POST / PUT `/trigger`** – all three methods are accepted for maximum webhook compatibility.
- ******** & `X-Api-Key` header authentication** – requests without a valid secret are rejected with `401 Unauthorized`.
- **Structured logging** – every request (including auth failures and unknown routes) produces a log line with method, path, source IP, and outcome.
- **Health endpoint** – `GET /health` returns `200 OK` for liveness/readiness probes.
- **Multi-arch Docker image** – built for `linux/amd64` and `linux/arm64` and published to GHCR.

---

## Configuration

| Environment variable | Required | Description |
|---|---|---|
| `API_SECRET` | ✅ | Shared secret. Callers must supply it as `Authorization: ****** or `X-Api-Key: <secret>`. |
| `CRON_JOB_NAME` | ✅ | Name of the existing Kubernetes CronJob to spawn jobs from. |
| `NAMESPACE` | optional | Kubernetes namespace. Defaults to the pod's own namespace (read from the service-account token). |
| `BIND_ADDR` | optional | Address to listen on. Defaults to `0.0.0.0:8080`. |

---

## API

### Trigger a Renovate run

```
GET|POST|PUT /trigger
Authorization: ******
```

or

```
GET|POST|PUT /trigger
X-Api-Key: <API_SECRET>
```

**Responses**

| Status | Meaning |
|---|---|
| `202 Accepted` | Job was created successfully. |
| `401 Unauthorized` | Missing or invalid secret. |
| `500 Internal Server Error` | Kubernetes API call failed. |

### Health check

```
GET /health
```

Returns `200 OK` with body `ok`.

---

## Deployment

### Kubernetes RBAC

A dedicated ServiceAccount, Role, and RoleBinding are provided in `k8s/rbac.yaml`.  
The Role grants only `create` on `jobs` and `get` on `cronjobs` within the target namespace.

### Helm / plain manifests

Ready-to-use manifests are in the `k8s/` directory:

| File | Contents |
|---|---|
| `k8s/rbac.yaml` | ServiceAccount, Role, RoleBinding |
| `k8s/deployment.yaml` | Deployment (references the ServiceAccount and the API secret) |
| `k8s/service.yaml` | ClusterIP Service |
| `k8s/ingress.yaml` | Ingress example |

Apply them in order:

```bash
kubectl apply -f k8s/rbac.yaml
kubectl create secret generic renovate-trigger-secret \
  --from-literal=API_SECRET=<your-secret>
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml
kubectl apply -f k8s/ingress.yaml
```

---

## GitHub webhook integration

1. Go to your GitHub repository (or organisation) **Settings → Webhooks → Add webhook**.
2. Set **Payload URL** to the public URL of the service, e.g. `https://renovate-trigger.example.com/trigger`.
3. Set **Content type** to `application/json`.
4. Set **Secret** — this is forwarded as `X-Hub-Signature-256`; the actual auth header must be configured separately (see below).
5. Add a custom header `X-Api-Key: <API_SECRET>` — GitHub webhooks support arbitrary headers since 2024.
6. Choose the events you care about (e.g. *Pushes*, *Releases*).

### GitHub Actions integration

```yaml
- name: Trigger Renovate
  run: |
    curl -sf -X POST \
      -H "Authorization: ****** secrets.RENOVATE_TRIGGER_SECRET }}" \
      https://renovate-trigger.example.com/trigger
```

---

## Building locally

```bash
cargo build --release
```

### Docker

```bash
docker build -t renovate-k8s-trigger .
docker run -e API_SECRET=secret -e CRON_JOB_NAME=renovate renovate-k8s-trigger
```

---

## License

MIT
