# Multi-stage build for Pancetta
# Stage 1: Builder
FROM rust:1.75-slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libasound2-dev \
    libssl-dev \
    cmake \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /usr/src/pancetta

# Copy Cargo files first for better caching
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY pancetta-audio ./pancetta-audio
COPY pancetta-core ./pancetta-core
COPY pancetta-dsp ./pancetta-dsp
COPY pancetta-ft8 ./pancetta-ft8
COPY pancetta-hamlib ./pancetta-hamlib
COPY pancetta-qso ./pancetta-qso
COPY pancetta-tui ./pancetta-tui
COPY pancetta-dx ./pancetta-dx
COPY pancetta-config ./pancetta-config
COPY pancetta ./pancetta

# Build release binary
RUN cargo build --release --bin pancetta

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libasound2 \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -s /bin/bash pancetta

# Copy binary from builder
COPY --from=builder /usr/src/pancetta/target/release/pancetta /usr/local/bin/pancetta

# Copy documentation
COPY docs /usr/share/doc/pancetta
COPY README.md /usr/share/doc/pancetta/

# Create config directory
RUN mkdir -p /etc/pancetta && \
    chown -R pancetta:pancetta /etc/pancetta

# Create default config
RUN cat > /etc/pancetta/config.toml << 'EOF' && \
[audio]
device_name = "default"
sample_rate = 48000
buffer_size = 512

[ft8]
decode_depth = 2
sensitivity = 0.5

[hamlib]
use_mock = true
host = "127.0.0.1"
port = 4532

[runtime]
worker_threads = 2

[logging]
level = "info"
file_logging = false
EOF
    chown pancetta:pancetta /etc/pancetta/config.toml

# Switch to non-root user
USER pancetta
WORKDIR /home/pancetta

# Create necessary directories
RUN mkdir -p ~/.config/pancetta ~/.local/share/pancetta ~/.cache/pancetta

# Environment variables
ENV RUST_LOG=info
ENV PANCETTA_CONFIG=/etc/pancetta/config.toml

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD pancetta --version || exit 1

# Default command (headless mode for container)
CMD ["pancetta", "--headless"]

# Expose metrics port (if enabled)
EXPOSE 9090

# Labels
LABEL org.opencontainers.image.title="Pancetta"
LABEL org.opencontainers.image.description="High-performance FT8 decoder for amateur radio"
LABEL org.opencontainers.image.version="1.0.0"
LABEL org.opencontainers.image.authors="Pancetta Team"
LABEL org.opencontainers.image.source="https://github.com/pancetta-project/pancetta"
LABEL org.opencontainers.image.licenses="MIT"