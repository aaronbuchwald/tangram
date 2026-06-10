# Sandbox image for tangram-nutrition (RUNTIME_PLAN Phase 0).
# Build context is the repo root; the static musl binary must already exist:
#   cargo build --release --target x86_64-unknown-linux-musl -p tangram-nutrition
#   docker build -f apps/nutrition/Dockerfile -t tangram/nutrition:dev .
# (or just scripts/build-images.sh, which does both)
#
# TLS roots for the calorieninjas strategy are compiled in (reqwest
# rustls-tls = webpki-roots), so no /etc/ssl is needed in the image.
FROM scratch
COPY target/x86_64-unknown-linux-musl/release/tangram-nutrition /tangram-nutrition
COPY apps/nutrition/ui /ui
ENV BIND_ADDR=0.0.0.0:8080 TANGRAM_DATA_DIR=/data TANGRAM_UI_DIR=/ui
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/tangram-nutrition"]
