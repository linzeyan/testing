# apitool

A small, portable Postman-style API client (Rust + egui) built for low-memory machines
such as Citrix VDI. No installer: unzip and run.

- **apitool** — the GUI. REST, GraphQL, WebSocket, SSE, MQTT and gRPC (runtime `.proto`;
  streaming methods connect like a WebSocket, and a JSON array body sends several
  messages); MQTT 3.1.1 or 5 over TCP, TLS or WebSocket (`mqtt://`, `mqtts://`, `ws://`,
  `wss://`) subscribes to the request's topics on Connect, follows changes to them while
  connected, and publishes from the message panel with QoS and retain; Postman-style pre-request/test
  scripts (`pm.*`, chai, `jsonSchema`); collection runner with CSV/JSON data (counts cover every row; the list keeps
  failures and the latest 1000 results); load test
  pane (response bodies are read and dropped, so many users cost no RAM); proxy/PAC/WPAD, custom CA and client certificates. JSON, text (Text/XML/HTML/JavaScript
  sets the Content-Type; XML has Beautify), form and multipart
  bodies (a value `@path` uploads a file), and a Binary body that sends a file as it is,
  streamed from disk with its type guessed from the extension (text past 128 KB, in a body,
  script, example or the Postman import box, is kept and sent but not laid out for
  editing: a 10 MB body took 3.6 GB to show); a description per param, header and form
  field, and Bulk Edit (`key: value` lines) for those tables; path variables (`/users/:id`
  gets an `id` row under Params, filled in on Send, kept on Postman import and export); per-request settings (HTTP
  version, redirects, TLS check, cookies, timeout); paste a curl command into the URL bar
  to import it, and "</> Code" for the request as curl, wget, HTTPie, PowerShell, raw
  HTTP, Python, fetch, axios, Go, Java, C#, PHP, Ruby, Rust, Swift or Kotlin (a Binary
  body reads its file, and `--data-binary @file` pastes back as one); import a
  Postman v2.1 collection or environment (⋯ > "Import from Postman…": paste it, give its
  path or drop the file) and copy a folder or the whole collection as a Postman
  collection; duplicate requests and folders (Ctrl+D); Save ⏷ "Save as…" saves the edits
  as a new request (`folder/name`; the original stays as saved); new request (Ctrl+N); Ctrl+L
  selects the URL; Ctrl+K jumps to any request (by folder and name, letters in order are
  enough) or environment; filter the tree by name; drag a request or folder onto a folder (or the
  empty space under the tree) to move it, open tabs following; a ⋯ on the hovered tree row
  opens the same menu as right-click; each request's last status code on its tree row; a
  `folder › request` breadcrumb whose folders open their settings; header names and Content-Type values complete while
  typing, and the Headers tab folds out what Send adds (Host, auth, Content-Type…) and
  where each comes from; a response body with JSON colours and line numbers, folding
  of pretty JSON objects and arrays (find opens a fold it lands in), Pretty/Raw (XML responses are indented too), a JSONPath filter (`$.items[*].id`, `..id`; Save… still writes the whole
  body), a Tests tab filter (All/Passed/Failed with counts), word wrap (between words) and Save… to a file (bodies past 16 MiB keep their start, to
  spare RAM; Send ⏷ "Send and download…" streams a successful response straight to a
  file instead, however big, and a failed or cancelled one leaves no partial file); a PNG or JPEG response shows as a picture, other binary
  bodies by their size (Save… writes them byte for byte), and "Open in browser" shows an
  HTML one; a Timeline tab with the request as it went out (headers reqwest adds and jar
  cookies included), each redirect and the peer address; a History menu in the
  response bar with the last 10 responses of each request (kept on disk, 1 MiB of body
  each); find in response (Ctrl+F; counts up to 10 000 hits); saved response
  examples; request history; Bearer, Basic, Digest, API key (header or query) and OAuth 2.0
  (client credentials, password, and authorization code with PKCE: Send opens the browser
  to sign in and catches the redirect on 127.0.0.1; an expired token is renewed with the
  refresh token when the provider gives one, without signing in again, and tokens are kept
  in the workspace across restarts) auth; a Cookies tab with what a response sets; a cookie jar like a browser's, with a manager; folder settings
  (right-click a folder) with variables, auth and scripts shared by every request inside,
  like Postman collection/folder settings; tabs (a click in the tree previews,
  double-click or editing keeps the tab, Ctrl+W closes; right-click a tab to duplicate it or
  close others, those to the right or all, which leaves tabs with unsaved edits open);
  hover the status code for what it means and the size for headers vs body; "Side by side"
  in the status bar puts the response beside the request and "Sidebar" folds the tree away
  (both kept across restarts); an environment can have a colour (prod in red) shown on its
  selector and as a strip across the top; Markdown API docs from
  request/folder descriptions and saved examples (right-click a folder > "Copy docs as
  Markdown", or `apitool-cli docs [folder] [-o api.md]`); a local mock server answering
  with saved examples (right-click a folder > "Start mock server", or
  `apitool-cli mock [folder] [--port 3000]`; `x-mock-response-name`/`-code` pick one).
- **apitool-cli** — headless runner for CI:
  `apitool-cli <folder|request> [-e env] [-d data.csv] [-n N] [--junit report.xml]`, with
  names as in the tree (`users/get user`; `.` is the whole collection); exit code 0 = all
  passed, 1 = failures, 2 = could not run.
- **apitool-cli mcp** — MCP server (stdio) so an LLM client can list, read, write, send and
  run requests and edit variables in the workspace. Secret values are masked unless asked for.

  ```json
  { "mcpServers": { "apitool": {
      "command": "C:\\tools\\apitool\\apitool-cli.exe",
      "args": ["mcp", "--workspace", "C:\\tools\\apitool\\workspace"] } } }
  ```

  The GUI re-reads the workspace when its window regains focus.

The workspace (`workspace/` next to the executable, or `$APITOOL_WORKSPACE`) keeps
everything in one SQLite file, `apitool.db`. For git, ⋯ > "Export to files" writes the
collection, environments and globals as TOML beside it (`collections/`, `environments/`,
`globals.toml`; secret values, history, cookies and OAuth tokens stay in the database), and "Import from
files…" reads them back, e.g. after a pull. A workspace from an older version, which was
those TOML files, is imported into `apitool.db` on first start, secrets and history
included.
