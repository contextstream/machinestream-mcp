# syntax=docker/dockerfile:1

# Build
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Bound the build's parallelism on a shared builder: --build-arg CARGO_BUILD_JOBS=8
ARG CARGO_BUILD_JOBS
RUN cargo build --release --locked --bin machinestream-mcp

# Run. git is needed at run time: a check starts from a snapshot of a git checkout.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    # A mounted checkout usually belongs to another user than the one in the container.
    && git config --system --add safe.directory '*' \
    && useradd --create-home --uid 10001 machinestream \
    && mkdir /workspace && chown machinestream /workspace
COPY --from=build /src/target/release/machinestream-mcp /usr/local/bin/machinestream-mcp
USER machinestream
WORKDIR /workspace
# With no arguments the server speaks MCP on stdin and stdout:
#   docker run -i --rm -e MACHINESTREAM_API_KEY -v "$PWD":/workspace machinestream-mcp
# Over HTTP, for container platforms and cloud agents (see docs/marketplaces.md):
#   docker run --rm -p 8808:8808 -v "$PWD":/workspace machinestream-mcp \
#     serve --http 0.0.0.0:8808 --root /workspace --allowed-host mcp.example.com
EXPOSE 8808
ENTRYPOINT ["machinestream-mcp"]
