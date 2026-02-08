# Drawio Collab Server

A collaborative draw.io server built with Rust and Axum.

## Quick Start

### Using Docker

```bash
# Build the image
docker build -t drawio_collab_server .

# Run the container (with random token)
docker run -p 3000:3000 -v $(pwd)/data:/app/data drawio_collab_server

# Run with a predefined token
docker run -p 3000:3000 -v $(pwd)/data:/app/data -e DRAWIO_TOKEN=your-secret-token drawio_collab_server
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
./target/release/drawio_collab_server
```

## Configuration

- `PORT`: Server port (default: 3000)
- `DRAWIO_TOKEN`: Authentication token
- `DRAWIO_ADMIN_TOKEN`: Admin token for admin panel access (default: random UUID)
- `DRAWIO_REQUIRE_TOKEN`: Require token for all operations (default: true)
- `DATA_DIR`: Directory for storing .drawio files (default: data)

## Features

- Collaborative editing with WebSocket support
- Git-based versioning
- File management API
- Token-based authentication
- Admin panel for Git remote configuration and scheduled pushes

## Admin Panel

The admin panel allows you to configure Git remote settings and schedule automatic pushes.

### Accessing the Admin Panel

1. Get your admin token from the server logs (or set `DRAWIO_ADMIN_TOKEN` environment variable)
2. Navigate to: `http://localhost:3000/admin.html?token=YOUR_ADMIN_TOKEN`

### Admin Features

- **Git Remote Configuration**: Set the remote URL for pushing changes to a Git repository
  - Configure repository URL and access token separately
  - Access tokens are automatically embedded in the remote URL
  - Token is masked in the UI for security
  - Supports GitHub (ghp_xxx), GitLab (glpat-xxx), and other token formats
- **Automatic Push Schedule**: Configure when and how often changes are automatically pushed to the remote repository
  - Enable/disable automatic pushes
  - Set push interval (minimum: 60 seconds)
  - Configure remote name and branch
  - **Push on Commit**: Automatically push immediately after each Git commit is created
- **Test Push**: Verify Git credentials are working correctly before enabling automatic pushes

### Adding Git Access Tokens

1. In the admin panel, enter your repository URL (e.g., `https://github.com/user/repo.git`)
2. Enter your access token in the "Access Token" field:
   - **GitHub**: Create a Personal Access Token (PAT) with `repo` scope (format: `ghp_xxxxxxxxxxxx`)
   - **GitLab**: Create a Personal Access Token with `write_repository` scope (format: `glpat-xxxxxxxxxxxx`)
   - **Other Git providers**: Use their respective token format
3. The token will be automatically embedded in the remote URL when you save
4. Use the "Test Push" button to verify your credentials work

**Note**: The token is stored in the Git remote URL. For security, the token field is cleared after saving and masked in the preview.

The admin token is separate from the regular authentication token and provides access to administrative functions only.

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0). See [LICENSE](LICENSE) for the full text.

