# apitool

A small, portable Postman-style API client (Rust + egui) built for low-memory machines
such as Citrix VDI. No installer: unzip and run.

- **apitool** — the GUI. REST, GraphQL (a `subscription` connects over WebSocket, graphql-transport-ws or the older graphql-ws, and streams each result), WebSocket, SSE, MQTT and gRPC (runtime `.proto`;
  streaming methods connect like a WebSocket, and a JSON array body sends several
  messages); MQTT 3.1.1 or 5 over TCP, TLS or WebSocket (`mqtt://`, `mqtts://`, `ws://`,
  `wss://`; TLS takes the network settings' CA file, client certificate and "accept any
  certificate", and the connection goes through the proxy https would use, tunnelled with
  CONNECT) subscribes to the request's topics on Connect, follows changes to them while
  connected, and publishes from the message panel with QoS and retain (and MQTT 5 user
  properties, which incoming messages show too); a last will
  (topic, message, QoS, retain) goes with the connection; Postman-style pre-request/test
  scripts (`pm.*`, chai, `jsonSchema`; `pm.variables.replaceIn`; `require` of Postman's sandbox modules: crypto-js, lodash, moment, uuid, tv4, chai, plus `atob`/`btoa`, evaluated only when required; `pm.cookies` and `pm.cookies.jar()` read and write the jar Send uses; `pm.sendRequest` with a callback or `await`, through the same clients, e.g. to fetch a token first; in the runner `pm.execution.setNextRequest(name)` (or `postman.`) jumps, loops or with `null` stops, and `pm.execution.skipRequest()` leaves a request out); an Asserts tab for checks without JS, as in Bruno (`res.status` `eq 200`, `res.body.items` `length 3`, `res.headers['content-type']` `contains json`; 28 operators, `{{variables}}` on the right; each row is a test result); `{{?name}}` prompt variables, asked for on Send and kept only until apitool closes (Insomnia's `{% prompt %}` imports as one); the post-response Snippets menu's "Response matches its schema" adds a `jsonSchema` contract test generated from it (array items merged, keys only some items have left optional); all of Postman's dynamic variables (`{{$guid}}`, `{{$timestamp}}`, `{{$randomEmail}}`, `{{$randomBankAccountIban}}`… 120 of them, a fresh value each use, `{{email` completes to `$randomEmail`); collection runner with CSV/JSON data (counts cover every row; the list keeps
  failures and the latest 1000 results); load test
  pane (response bodies are read and dropped, so many users cost no RAM); proxy/PAC/WPAD (http, https and SOCKS proxies: `socks5h://host:1080`, or a PAC `SOCKS`/`SOCKS5` entry), custom CA and client certificates. JSON, text (Text/XML/HTML/JavaScript
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
  Postman v2.1 collection or environment, or an OpenAPI 3 / Swagger 2 spec in JSON or YAML
  (a request per operation, a folder per tag, `{{baseUrl}}` from the server, auth from the
  security schemes, bodies and saved examples from the schemas, so the mock server answers
  right away), an Insomnia export (v4 JSON or v5 YAML: folders, auth, scripts, `{{ _.x }}`
  as `{{x}}`, the base environment as collection variables and each sub-environment as an
  environment), or a HAR from browser devtools (a folder per host, each response kept as
  the request's example) (⋯ > "Import…": paste it, give its path or drop the file) and copy a folder or the whole collection as a Postman
  collection; duplicate requests and folders (Ctrl+D); Save ⏷ "Save as…" saves the edits
  as a new request (`folder/name`; the original stays as saved); new request (Ctrl+N); Ctrl+L
  selects the URL; Ctrl+K jumps to any request (by folder and name, letters in order are
  enough), folder (its settings) or environment, or does what a button does (new request,
  network settings, cookies, side by side…); filter the tree by name; drag a request or folder onto a folder (or the
  empty space under the tree) to move it, open tabs following; a ⋯ on the hovered tree row
  opens the same menu as right-click; each request's last status code on its tree row; a
  `folder › request` breadcrumb whose folders open their settings; header names and Content-Type values complete while
  typing, and the Headers tab folds out what Send adds (Host, auth, Content-Type…) and
  where each comes from; a response body with JSON colours and line numbers, folding
  of pretty JSON objects and arrays (find opens a fold it lands in), Pretty/Raw (XML responses are indented too; Raw, once chosen, sticks to that content type), a JSONPath filter (`$.items[*].id`, `..id`, `$.items[?(@.price < 10 && @.tag == 'x')]`; the last 10 used come back from Recent; Save… still writes the whole
  body), a Tests tab filter (All/Passed/Failed with counts), word wrap (between words) and Save… to a file (bodies past 16 MiB keep their start, to
  spare RAM; Send ⏷ "Send and download…" streams a successful response straight to a
  file instead, however big, and a failed or cancelled one leaves no partial file); a PNG or JPEG response shows as a picture, other binary
  bodies by their size (Save… writes them byte for byte), and "Open in browser" shows an
  HTML one; a Timeline tab with the request as it went out (headers reqwest adds and jar
  cookies included), each redirect and the peer address; a History menu in the
  response bar with the last 10 responses of each request (kept on disk, 1 MiB of body
  each); find in response (Ctrl+F; match case, whole word or regular expression; counts up to 10 000 hits); saved response
  examples; request history; Bearer, Basic, Digest, API key (header or query), AWS Signature v4 (signed over the final URL, headers and body on every send; S3 and session tokens included; Postman's awsv4 both ways) and OAuth 2.0
  (client credentials, password, implicit, and authorization code with PKCE: Send opens the browser
  to sign in and catches the redirect on 127.0.0.1, and implicit takes its token from that redirect; an expired token is renewed with the
  refresh token when the provider gives one, without signing in again, and tokens are kept
  in the workspace across restarts) auth; a Cookies tab with what a response sets; a cookie jar like a browser's, with a manager; folder settings
  (right-click a folder) with variables, auth and scripts shared by every request inside,
  like Postman collection/folder settings; tabs (a click in the tree previews,
  double-click or editing keeps the tab, Ctrl+W closes; right-click a tab to duplicate it or
  close others, those to the right or all, which leaves tabs with unsaved edits open);
  until something is sent, the response pane lists the keyboard shortcuts; hover the status code for what it means, the time for opening the connection (DNS, TCP and TLS together), waiting (TTFB) and download, and the size for headers vs body; the response sits beside the request (a window too narrow for two columns stacks them
  until it is wider; "Side by side" in the status bar turns it off) and "Sidebar" folds the tree away
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
