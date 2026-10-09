# apitool

English | [正體中文](README.zh-TW.md)

A small, portable Postman-style API client written in Rust with egui, built for low-memory
machines such as Citrix VDI. No installer: unzip and run.

- **GUI, CLI and MCP**: the `apitool` window, `apitool-cli` for CI, and an MCP server for LLM clients.
- **Speaks Postman**: imports Postman, OpenAPI, Insomnia and HAR; runs `pm.*` scripts.
- **Light on memory**: big bodies are capped or streamed to disk; load tests drop response bodies.
- **Git-friendly**: collections are plain TOML; optional sync through a GitHub or GitLab repository.

## Install

Download the archive for your system from
[Releases](https://github.com/linzeyan/testing/releases/latest), unpack it anywhere and run
`apitool`.

| System | Archive |
|---|---|
| Windows x64 | `apitool-v<version>-x86_64-pc-windows-msvc.zip` |
| macOS, Apple silicon | `apitool-v<version>-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `apitool-v<version>-x86_64-apple-darwin.tar.gz` |
| Linux x64 | `apitool-v<version>-x86_64-unknown-linux-gnu.tar.gz` |

Each archive holds `apitool` (the GUI) and `apitool-cli` (runner, docs, mock server and
MCP server). apitool looks for a newer release once a day and offers to install it in
place, used from the next start; Settings > Updates sets how often (never, at start, daily,
weekly) and whether to install without asking. The interface is in English or Traditional
Chinese (Settings > General).

To build from source with stable Rust:

```sh
cargo build --release --locked   # target/release/apitool and apitool-cli
```

## Protocols

| Protocol | Notes |
|---|---|
| HTTP | HTTP version, redirects, TLS check, cookies and timeout per request |
| GraphQL | A `subscription` connects over WebSocket (graphql-transport-ws or the older graphql-ws) and streams each result |
| WebSocket | |
| SSE | |
| Socket.IO 4 | The URL's path is the namespace and a JSON body the auth payload; send `event {"json": "arg"}` or a `["event", …]` array; acks are shown |
| MQTT 3.1.1 and 5 | Over TCP, TLS or WebSocket (`mqtt://`, `mqtts://`, `ws://`, `wss://`), see below |
| gRPC | Methods from a `.proto` loaded at run time or, with none set, from server reflection (v1 or v1alpha, asked on ↻ or the first send). Streaming methods connect like a WebSocket; a JSON array body sends several messages |

MQTT:

- Subscribes to the request's topics on Connect and follows changes to them while connected.
- Publishes from the message panel with QoS and retain. MQTT 5 user properties go out with
  a message and show on incoming ones.
- A last will (topic, message, QoS, retain) goes with the connection.
- TLS takes the network settings' CA file, client certificate and "accept any certificate".
  The connection goes through the proxy https would use, tunnelled with CONNECT.

Every stream's log narrows to received or sent messages (↓ ↑) and to text, which keeps up
with a busy feed. The bookmark under Send keeps the typed message in the request; the list
beside it puts a kept one back in the box.

## Requests

**Bodies**

- JSON; text (Text, XML, HTML or JavaScript sets the Content-Type; XML has Beautify); form;
  multipart (a value `@path` uploads a file).
- Binary sends a file as it is, streamed from disk, its type guessed from the extension.
- Text past 128 KB (in a body, script, example or the Postman import box) is kept and sent
  but not laid out for editing: a 10 MB body took 3.6 GB to show.

**Params, headers and variables**

- A description per param, header and form field; Bulk Edit edits those tables as
  `key: value` lines.
- Path variables: `/users/:id` (or OpenAPI's `/users/{id}`, as copied from Swagger UI) gets
  an `id` row under Params, is coloured in the URL bar, filled in on Send and kept on Postman
  import and export.
- Header names and Content-Type values complete while typing. The Headers tab folds out what
  Send adds (Host, auth, Content-Type…) and where each comes from.
- All 120 of Postman's dynamic variables (`{{$guid}}`, `{{$timestamp}}`, `{{$randomEmail}}`,
  `{{$randomBankAccountIban}}`…), a fresh value each use; `{{email` completes to
  `$randomEmail`.
- `{{?name}}` prompt variables are asked for on Send and kept only until apitool closes;
  Insomnia's `{% prompt %}` imports as one.
- An environment can have a colour (prod in red), shown on its selector and as a strip
  across the top.
- Folder settings (right-click a folder) hold variables, auth and scripts shared by every
  request inside, like Postman's collection and folder settings.

**Network**

- Proxies: http, https and SOCKS (`socks5h://host:1080`), PAC (`SOCKS`/`SOCKS5` entries
  included) and WPAD.
- A custom CA, and client certificates in PEM or PFX/P12: a default one plus per-host ones
  (`api.corp.com`, `*.corp.com`, `host:8443`).
- A cookie jar like a browser's, with a manager.

**Sending**

- Paste a curl command into the URL bar to import it.
- Send ⏷ "Send and download…" streams a successful response straight to a file, however
  big; a failed or cancelled one leaves no partial file.
- Send ⏷ "Repeat every N s" sends again on that interval, never two at once, until Stop
  repeat, Cancel or another tab.

## Authentication

| Type | Notes |
|---|---|
| Bearer, Basic, Digest | |
| API key | In a header or the query |
| AWS Signature v4 | Signed over the final URL, headers and body on every send; S3 and session tokens included; Postman's awsv4 both ways |
| OAuth 1.0a | HMAC-SHA1/256/512, RSA-SHA256/512, PLAINTEXT; signed over method, URL, query and form body; Postman and Insomnia import |
| JWT Bearer | HS/RS/PS/ES 256–512 and EdDSA, signed on each send from a JSON payload with variables; Postman's jwt both ways |
| OAuth 2.0 | Client credentials, password, implicit, and authorization code with PKCE |

For authorization code and implicit, Send opens the browser to sign in and catches the
redirect on 127.0.0.1. An expired token is renewed with the refresh token when the provider
gives one, without signing in again. Tokens are kept in the workspace across restarts.

## Scripts and tests

Pre-request and test scripts in Postman's style, empty until written ("Insert template" and
Snippets fill some in):

- `pm.*`, chai, `jsonSchema`, `pm.variables.replaceIn`.
- `require` of Postman's sandbox modules (crypto-js, lodash, moment, uuid, tv4, chai), plus
  `atob`/`btoa`; each is evaluated only when required.
- `pm.cookies` and `pm.cookies.jar()` read and write the jar Send uses.
- `pm.sendRequest` with a callback or `await`, through the same clients, e.g. to fetch a
  token first.
- In the runner, `pm.execution.setNextRequest(name)` (or `postman.setNextRequest`) jumps or
  loops, and with `null` stops; `pm.execution.skipRequest()` leaves a request out.

Without JavaScript:

- The Asserts tab, as in Bruno: rows like `res.status` `eq 200`, `res.body.items` `length 3`,
  `res.headers['content-type']` `contains json`. 28 operators, `{{variables}}` on the right;
  each row is a test result.
- The Vars tab, Bruno's post-response vars: `token` `res.body.access_token` sets `token` in
  the selected environment once the response is in, before the scripts and Asserts, so a
  login hands its token on without a line of JavaScript. A row that fails or finds nothing
  shows up under Tests.
- The post-response Snippets menu's "Response matches its schema" adds a `jsonSchema`
  contract test generated from the response (array items merged; keys only some items have
  are left optional).

The Tests tab filters All / Passed / Failed, with counts.

## Responses

**Reading**

- JSON with colours, line numbers and folding of pretty objects and arrays (find opens a
  fold it lands in). Word wrap breaks between words.
- Pretty / Raw. XML responses are indented too; Raw, once chosen, sticks to that content type.
- Filter with JSONPath (`$.items[*].id`, `..id`, `$.items[?(@.price < 10 && @.tag == 'x')]`)
  or, on XML, XPath (`//item[@id='b']/name/text()`, prefixes ignored). The last 10 come back
  from Recent; Save… still writes the whole body.
- Find (Ctrl+F): match case, whole word or regular expression; counts up to 10 000 hits.
- PNG, JPEG and WebP show as pictures, SVG too (Preview, beside Pretty and Raw), PDF page by
  page; other binary bodies show their size. "Open in browser" shows an HTML, PDF or SVG one
  (a PDF in the system's viewer).

**Details**

- Hover the status code for what it means, the time for DNS lookup, TCP connect, TLS
  handshake, waiting (TTFB) and download, and the size for headers vs body.
- The Timeline tab: the request as it went out (headers the HTTP client adds and jar cookies
  included), each redirect and the peer address.
- The Cookies tab: what the response sets.

**Keeping**

- Save… writes the body to a file (binary bodies byte for byte).
- The History menu in the response bar: the last 10 responses of each request, kept on disk
  with 1 MiB of body each. Request history and saved response examples too.

**Big bodies**: text bodies past 5 MiB wait behind "Show anyway / Save to file…"; bodies
past 16 MiB keep only their start, to spare RAM. Send and download keeps a whole one.

## Collections, tabs and layout

**Tree**

- Filter by name. Each request's row shows its last status code.
- Drag a request or folder onto a folder (or the empty space under the tree) to move it,
  open tabs following, or onto a row's top or bottom edge to put it before or after that
  row. The order is kept in `.folder.toml`; Postman and Insomnia imports keep theirs.
- The ⋯ on a hovered row opens the same menu as right-click.
- A new request goes into the folder last clicked in the tree, else beside the open request;
  its dialog can pick another folder.
- Save ⏷ "Save as…" saves the edits as a new request (`folder/name`); the original stays as
  saved.
- A `folder › request` breadcrumb, whose folders open their settings.

**Tabs**: a click in the tree previews a request; a double-click or an edit keeps its tab. A
middle click closes one. Right-click a tab to duplicate it, or to close others, those to the
right or all, which leaves tabs with unsaved edits open.

**Layout**: the response sits beside the request; a window too narrow for two columns stacks
them until it's wider. "Side by side" in the status bar turns that off and "Sidebar" folds
the tree away; both are kept across restarts.

## Keyboard shortcuts

Ctrl is ⌘ on macOS, except for switching tabs. Until something is sent, the response pane
lists these too.

| Action | Keys |
|---|---|
| Send request | `Ctrl+Enter` |
| Send and download… | `Ctrl+Shift+Enter` |
| Save changes | `Ctrl+S` |
| Save as… | `Ctrl+Shift+S` |
| Go to a request, folder or action | `Ctrl+K` |
| New request | `Ctrl+N` |
| New collection | `Ctrl+Shift+N` |
| Import… | `Ctrl+O` |
| Duplicate request or folder | `Ctrl+D` |
| Rename the folder clicked in the tree, else the open request | `F2` |
| Delete what's picked or clicked in the tree | `Delete` (`⌘⌫` on macOS) |
| Select the URL | `Ctrl+L` |
| Code | `Ctrl+Shift+G` |
| Edit environment | `Ctrl+E` |
| Find in the response | `Ctrl+F` |
| Close tab | `Ctrl+W` |
| Reopen closed tab, where it was | `Ctrl+Shift+T` |
| Next / previous tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Go to tab 1–8, the last | `Ctrl+1`…`9` |
| Sidebar | `Ctrl+\` |
| Side by side | `Ctrl+Alt+V` |

Ctrl+K finds a request by folder and name (letters in order are enough), a folder (its
settings) or an environment, and does what a button does (new request, network settings,
cookies, side by side…).

## Import and export

Import from ⋯ > "Import…": paste it, give its path or drop the file.

| Format | What comes in |
|---|---|
| curl | Paste it into the URL bar instead |
| Postman v2.1 | A collection or an environment |
| OpenAPI 3 / Swagger 2, JSON or YAML | A request per operation, a folder per tag, `{{baseUrl}}` from the server, auth from the security schemes, bodies and saved examples from the schemas, so the mock server answers right away |
| Insomnia v4 JSON / v5 YAML | Folders, auth and scripts; `{{ _.x }}` as `{{x}}`; the base environment as collection variables and each sub-environment as an environment |
| HAR from browser devtools | A folder per host, each response kept as the request's example |

Copy a folder or the whole collection as a Postman collection.

"</> Code" writes the request as curl, wget, HTTPie, PowerShell, raw HTTP, Python, fetch,
axios, Go, Java, C#, PHP, Ruby, Rust, Swift or Kotlin. A Binary body reads its file, and
`--data-binary @file` pastes back as one. Streams get their own clients; the language picked
last carries over between kinds.

| Kind | Clients |
|---|---|
| SSE | curl, fetch, requests, Go |
| WebSocket | websocat, browser, ws, websockets, gorilla |
| Socket.IO | socket.io-client, python-socketio |
| MQTT | mosquitto, paho-mqtt, mqtt.js, paho Go |
| gRPC | grpcurl, grpc-js, grpcio, by the method's streaming shape |

Markdown API docs come from request and folder descriptions and saved examples:
right-click a folder > "Copy docs as Markdown", or `apitool-cli docs`.

## Runner, load test and mock server

- **Collection runner** with CSV or JSON data. Counts cover every row; the list keeps
  failures and the latest 1000 results. Tick the requests to run and drag them into the
  order wanted (kept per folder on this machine; ↺ goes back to the tree's). "Stop at the
  first failure" ends the run there. The export icon writes the results as JUnit XML.
- **Load test** pane. Response bodies are read and dropped, so many users cost no RAM.
- **Mock server** answering with saved examples: right-click a folder > "Start mock server",
  or `apitool-cli mock`. An `x-mock-response-name` or `x-mock-response-code` header picks
  the example.

## Command line

```
apitool-cli <folder|request> [options]   run requests and their tests
apitool-cli docs [folder] [-o api.md]     Markdown API docs (default: stdout)
apitool-cli mock [folder] [--port 3000]   mock server on localhost
apitool-cli mcp [--window]                MCP server over stdio
```

Every command takes `--workspace <dir>`. Requests are named as in the tree (`users`,
`users/get user`; `.` is the whole collection).

| Option | |
|---|---|
| `-e, --env <name>` | Environment to use |
| `-d, --data <file>` | CSV or JSON data file; one iteration per row |
| `-n, --iterations <n>` | Iterations without a data file (default 1) |
| `--delay <ms>` | Pause between requests |
| `--bail` | Stop at the first request that fails |
| `--junit <file>` | Also write a JUnit XML report |

Exit code: `0` all passed, `1` something failed, `2` could not run.

## MCP server

`apitool-cli mcp` lets an LLM client list, read, write, move, delete, send and run requests,
edit folders and variables, import collections and specs, generate code, read the history
and listen to streams (SSE, WebSocket, Socket.IO, MQTT, gRPC) in the workspace. Secret
values are masked unless asked for, in snippets too.

Settings > MCP gives the setup to copy for Claude Code, Claude Desktop or Cursor:

```json
{ "mcpServers": { "apitool": {
    "command": "C:\\tools\\apitool\\apitool-cli.exe",
    "args": ["mcp", "--workspace", "C:\\tools\\apitool\\workspace"] } } }
```

The GUI re-reads the workspace when its window regains focus.

With "Let it operate this window" on, the window serves MCP itself (a new token each start,
web pages refused) and `apitool-cli mcp --window` relays to it. It listens on 127.0.0.1 and a
new free port each start unless Settings gives a host and port. A client that connects
over HTTP instead posts to the URL Settings shows, with the token shown there (copy icon
beside it) as `Authorization: Bearer`, over HTTP/1.1 or HTTP/2 without TLS (h2c, with prior
knowledge). The
client then also sees what's open (unsaved edits, the response, a live stream), opens
requests, presses Send and switches environments, and its changes show at once.

## Workspace and data

| | macOS and Linux | Windows |
|---|---|---|
| Workspace | `$APITOOL_WORKSPACE`, else `$XDG_DATA_HOME/apitool` (`~/.local/share/apitool`) | `$APITOOL_WORKSPACE`, else `workspace/` next to the executable |
| Log, `apitool.log` | `$XDG_STATE_HOME/apitool` (`~/.local/state/apitool`) | The workspace |

The workspace keeps everything in one SQLite file, `apitool.db`. The collections,
environments and globals are also written beside it as TOML for git (`collections/`,
`environments/`, `globals.toml`), kept in step both ways without anything to click.

Secret values, history, cookies and OAuth tokens stay in the database. Secret values are
sealed by the OS (DPAPI on Windows, a login-keychain key on macOS, plain on Linux), so a
database copied to another user or machine can't open them.

## Sync

Settings > Sync keeps the workspace tree in a private GitHub.com or GitLab.com repository,
so two machines share one workspace.

1. Give the repository (`owner/name` or its address), a branch and a token, kept sealed
   like secret values. "Create a token" opens the provider's page with this filled in:
   - GitHub: fine-grained, Contents read and write on that repository.
   - GitLab: the `api` scope.
2. ↻ in the status bar pulls what changed there and pushes what changed here in one commit,
   through the proxy set for requests. "Sync automatically" also does it when apitool starts
   and then every 5, 15, 30 or 60 minutes.

A request changed on both sides since the last sync is asked about: keep the repository's or
this machine's.

| Data | Synced |
|---|---|
| Collections, environments, globals | Always |
| Secret values, history, cookies, OAuth tokens | Only those ticked under "Also sync" |
| Past response bodies, UI state | Never |

Ticked ones go encrypted with a passphrase (the same on every machine; PBKDF2 and
AES-256-GCM) into `.apitool/` in the repository, never into the workspace folder, and merge
entry by entry: sends on two machines add up, secret values changed on both are asked about,
and cookies and tokens changed on both keep each machine's.

Other files in the repository are left alone.
