# Static musl binary — no shell/apt needed at image build time.
# COPY-only layers work for multi-arch without QEMU.
FROM gcr.io/distroless/static-debian12:nonroot

ARG TARGETARCH
COPY dist/${TARGETARCH}/renovate-k8s-trigger /usr/local/bin/renovate-k8s-trigger

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/renovate-k8s-trigger"]
