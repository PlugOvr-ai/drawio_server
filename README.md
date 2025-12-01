# Draw.io Server

A collaborative draw.io server built with Rust and Axum.

## Quick Start

### Using Docker

```bash
# Build the image
docker build -t drawioserver .

# Run the container (with random token)
docker run -p 3000:3000 -v $(pwd)/data:/app/data drawioserver

# Run with a predefined token
docker run -p 3000:3000 -v $(pwd)/data:/app/data -e DRAWIO_TOKEN=your-secret-token drawioserver
```

Access the server at `http://localhost:3000`

### Local Development

```bash
# Download and extract drawio.war
mkdir -p static/diagrams
curl -L -o /tmp/draw.war https://github.com/jgraph/drawio/releases/download/v24.0.0/draw.war
unzip -q /tmp/draw.war -d static/diagrams
cp PostConfig.js static/diagrams/js
rm /tmp/draw.war

# Build
cargo build --release

# Run
./target/release/drawioserver
```

## Configuration

- `PORT`: Server port (default: 3000)
- `DRAWIO_TOKEN`: Authentication token
- `DRAWIO_REQUIRE_TOKEN`: Require token for all operations (default: true)
- `DATA_DIR`: Directory for storing .drawio files (default: data)

## Features

- Collaborative editing with WebSocket support
- Git-based versioning
- File management API
- Token-based authentication

