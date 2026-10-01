# apitool

A small, portable Postman-style API client (Rust + egui) built for low-memory machines
such as Citrix VDI. No installer: unzip and run.

- **apitool** — the GUI. REST, GraphQL, WebSocket, SSE and unary gRPC (runtime `.proto`);
  Postman-style pre-request/test scripts (`pm.*`, chai, `jsonSchema`); collection runner with
  CSV/JSON data; load test pane; proxy/PAC, custom CA and client certificates.
- **apitool-cli** — headless runner for CI:
  `apitool-cli <collection|folder|request.toml> [-e env] [-d data.csv] [-n N]`,
  exit code 0 = all passed, 1 = failures, 2 = could not run.
- **apitool-cli mcp** — MCP server (stdio) so an LLM client can list, read, write, send and
  run requests and edit variables in the workspace. Secret values are masked unless asked for.

  ```json
  { "mcpServers": { "apitool": {
      "command": "C:\\tools\\apitool\\apitool-cli.exe",
      "args": ["mcp", "--workspace", "C:\\tools\\apitool\\workspace"] } } }
  ```

  The GUI re-reads the workspace when its window regains focus.

The workspace (`workspace/` next to the executable, or `$APITOOL_WORKSPACE`) is plain TOML
meant to be a git repo; `*.secret.toml` and `.state.toml` stay local.
