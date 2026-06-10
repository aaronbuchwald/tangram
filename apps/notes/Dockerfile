# Sandbox image for tangram-notes (RUNTIME_PLAN Phase 0).
# Build context is the repo root; the static musl binary must already exist:
#   cargo build --release --target x86_64-unknown-linux-musl -p tangram-notes
#   docker build -f apps/notes/Dockerfile -t tangram/notes:dev .
# (or just scripts/build-images.sh, which does both)
FROM scratch
COPY target/x86_64-unknown-linux-musl/release/tangram-notes /tangram-notes
COPY apps/notes/ui /ui
ENV BIND_ADDR=0.0.0.0:8080 TANGRAM_DATA_DIR=/data TANGRAM_UI_DIR=/ui
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/tangram-notes"]
