# One image, two binaries (peakpilot-cloud, gateway-sim). Pick one with the command.

# ---- build ---------------------------------------------------------------
FROM rust:1-slim-bookworm AS build
WORKDIR /src

# Cache dependencies separately from our code: copy manifests, build with stub
# sources, then copy the real sources. Only the last layer rebuilds on code changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/ems-core/Cargo.toml crates/ems-core/
COPY crates/cloud/Cargo.toml crates/cloud/
COPY crates/gateway-sim/Cargo.toml crates/gateway-sim/
RUN mkdir -p crates/ems-core/src crates/cloud/src crates/gateway-sim/src \
 && echo "" > crates/ems-core/src/lib.rs \
 && echo "fn main() {}" > crates/cloud/src/main.rs \
 && echo "fn main() {}" > crates/gateway-sim/src/main.rs \
 && cargo build --release --locked \
 && cargo clean --release -p ems-core -p cloud -p gateway-sim \
 && rm -rf crates/*/src

COPY crates crates
RUN cargo build --release --locked \
 && cp target/release/peakpilot-cloud target/release/gateway-sim /usr/local/bin/

# ---- runtime ---------------------------------------------------------------
# distroless: no shell, no package manager, runs as non-root. ~30 MB total.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /usr/local/bin/peakpilot-cloud /usr/local/bin/gateway-sim /usr/local/bin/
# Numeric UID (distroless "nonroot" = 65532): Kubernetes runAsNonRoot can only
# verify a numeric user, a named one fails with CreateContainerConfigError.
USER 65532:65532
EXPOSE 8080
ENTRYPOINT []
CMD ["/usr/local/bin/peakpilot-cloud"]
