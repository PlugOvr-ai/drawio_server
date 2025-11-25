# Build stage
FROM rust:1.89-slim AS builder

WORKDIR /app

# Install dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy manifest files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    unzip \
    && rm -rf /var/lib/apt/lists/*

# Download and extract drawio.war
RUN mkdir -p static/diagrams && \
    curl -L -o /tmp/draw.war https://github.com/jgraph/drawio/releases/download/v24.0.0/draw.war && \
    unzip -q /tmp/draw.war -d static/diagrams && \
    rm /tmp/draw.war

# Copy static HTML files
COPY static/drawio.html static/index.html static/token.html static/

# Copy the binary from builder
COPY --from=builder /app/target/release/drawioserver /app/drawioserver

# Create data directory
RUN mkdir -p data

# Expose port
EXPOSE 3000

# Set environment variables
ENV PORT=3000
ENV DATA_DIR=data

# Run the server
CMD ["./drawioserver", "--data-dir", "data"]

