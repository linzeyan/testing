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
| Linux arm64 | `apitool-v<version>-aarch64-unknown-linux-gnu.tar.gz` |

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
| GraphQL | Once the schema is fetched, fields complete as you type the query; variables fill in as a form, a row per `$variable` the query declares (enum and Boolean pickers, numbers that may be `{{variables}}`; an input object lists every field of its type, nested ones under the ticked fields, and only ticked ones are sent, as in Postman), or as JSON. A variable or field added starts with a random value of its type, as gRPC's Fill body does (Fill in values off: empty); under the query with a divider to drag, so a long query doesn't push them off screen; a `subscription` connects over WebSocket (graphql-transport-ws or the older graphql-ws) and streams each result. The schema explorer lists queries, mutations and subscriptions. A query holding several operations shows a picker beside "Query" for the one to run, sent as `operationName`. Beautify lays a one-line query out one field per line (a query with `#` comments is left as it is) |
| WebSocket | Text or binary messages: beside Send, Base64 or Hex sends the box decoded, as one binary message; received ones show their size and bytes in hex |
| SSE | |
| Socket.IO 2, 3 and 4 | The URL's path is the namespace and a JSON body the auth payload (v2 has none: Params go on the handshake's query); Settings sets the handshake path (`/socket.io/` if empty) and the client version; send `event {"json": "arg"}` or a `["event", …]` array, or name the event in the Event field and type only its argument; Ack asks the server to acknowledge a message, and its answer shows as `ack` with the same number |
| MQTT 3.1.1 and 5 | Over TCP, TLS or WebSocket (`mqtt://`, `mqtts://`, `ws://`, `wss://`), see below |
| gRPC | Methods from a `.proto` loaded at run time or, with none set, from server reflection (v1 or v1alpha, asked on ↻ or the first send). Streaming methods connect like a WebSocket; a JSON array body sends several messages |

MQTT:

- Subscribes to the request's topics on Connect and follows changes to them while connected.
- Publishes from the message panel with QoS and retain. MQTT 5 properties (content type,
  response topic, expiry, user properties) go out with a message and show on incoming ones.
- A last will (topic, message, QoS, retain; in MQTT 5 also content type and expiry) goes
  with the connection.
- TLS takes the network settings' CA file, client certificate and "accept any certificate".
  The connection goes through the proxy https would use (http:// or https://), tunnelled
  with CONNECT.

Every stream's log narrows to received or sent messages (↓ ↑) and to text, which keeps up
with a busy feed. Each message shows the local time it came or went, to the millisecond;
hovering shows the seconds since connecting. The message box is there before Connect, so a message can be written
first; Send waits for the connection. A message that can't go out (JSON that doesn't parse,
a typo in Base64) says why under the box. The bookmark under Send keeps the typed message in
the request; the list beside it puts a kept one back in the box. Clicking a message shows its
payload below, to copy, as indented JSON even after a topic or event name (`chat {…}`); ×
closes it.

## Requests

**Bodies**

- JSON (Beautify, or Minify to one line); text (Text, XML, HTML or JavaScript sets the
  Content-Type; XML has Beautify); form;
  multipart (a value `@path` uploads a file; each part's 📂 picks one).
- Binary sends a file as it is, streamed from disk, its type guessed from the extension;
  or bytes pasted as Base64 (standard or URL-safe) or hex, decoded when sent. The size they
  decode to shows under them, a typo in orange; switching between the two keeps the bytes.
- GraphQL is a request type of its own (GRAPHQL in the method list), not a body type.
  Switching a GraphQL request to another method turns its query into the JSON body it
  sends, and switching back reads the query out of it again.
- Text past 128 KB (in a body, script, example or the Postman import box) is kept and sent
  but not laid out for editing: a 10 MB body took 3.6 GB to show.

**Params, headers and variables**

- A description per param, header and form field; Bulk Edit edits those tables as
  `key: value` lines. Filled tables keep Key / Value / Description titles over their columns.
- A `{{variable}}` nothing defines is named beside the request's tabs, with "Define in…"
  to add it to the active environment (or the globals).
- Path variables: `/users/:id` (or OpenAPI's `/users/{id}`, as copied from Swagger UI) gets
  an `id` row under Params, is coloured in the URL bar, filled in on Send and kept on Postman
  import and export.
- Right-click selected text in a box that takes `{{variables}}`: cut, copy, paste, encode or
  decode it as a URI component, or put it in a variable (new or existing, in the active environment
  or the globals); the text itself stays as it is, as in Postman. A new variable is secret
  (kept on this machine) unless its Secret box is unticked.
- Header names and Content-Type values complete while typing. The Headers tab folds out what
  Send adds (Host, auth, Content-Type…) in the table's columns, credentials masked, and where
  each comes from; "Change the defaults…" opens
  Settings > Network, where User-Agent, Accept and gzip's Accept-Encoding are set for every
  request that doesn't set its own. An empty User-Agent sends none; an empty Accept still sends
  `*/*`, which the HTTP client always adds.
- All 120 of Postman's dynamic variables (`{{$guid}}`, `{{$timestamp}}`, `{{$randomEmail}}`,
  `{{$randomBankAccountIban}}`…), a fresh value each use; `{{email` completes to
  `$randomEmail`.
- `{{?name}}` prompt variables are asked for on Send and kept only until apitool closes;
  Insomnia's `{% prompt %}` imports as one.
- Quick look (the eye by the environment) docks a panel of every variable in scope, grouped as
  environment, folders and globals, the overridden ones struck through. Environment and global
  values edit in place: Enter keeps the change, Esc puts it back. Secret values show as dots
  until its eye shows them, as in the environment editor.
- The environment list edits any environment (✎) without switching to it, and ends with
  "New environment".
- An environment can have a colour (prod in red), shown on its selector and as a strip
  across the top.
- Folder settings (right-click a folder) hold variables, auth and scripts shared by every
  request inside, like Postman's collection and folder settings. A request's inherited auth
  names the folder it comes from and opens that folder's Auth tab. A variable set in several
  places takes the value nearest the request: globals < environment < collection < folder
  (inner over outer) < a run's data row < `pm.variables`. As Insomnia and Hoppscotch have it;
  Postman puts the environment over the collection.

**Network** (Settings > Network; the status bar shows the proxy in use and opens it). Changes
take effect on Apply (or Enter); closing Settings without it drops them.

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
- A request's Settings, as in Postman: keep the method and body on a 301, 302 or 303; keep
  Authorization when a redirect goes to another host; drop the Referer on redirects; turn
  off TLS 1.2 or 1.3; pick the cipher suites offered; turn off the strict HTTP parser for
  servers that send malformed headers. Postman's `protocolProfileBehavior` and curl's
  `--post301`, `--location-trusted`, `--tlsv1.3`, `--ciphers` carry them.
- Every kind of request has a Settings tab with what its client uses: the certificate
  check, cookies, TLS versions and cipher suites wherever they apply; the WebSocket and
  Socket.IO handshake timeout; how long SSE waits for the stream to start; gRPC's deadline
  (also sent as `grpc-timeout`) and replies that list the fields at their default values;
  MQTT's connect timeout; and the largest message a WebSocket, Socket.IO, gRPC or MQTT
  connection takes in. A WebSocket, Socket.IO, SSE or MQTT connection that ends without
  Disconnect can open again, up to a set number of times in a row at a set interval: SSE
  sends the last event ID as `Last-Event-ID`, and MQTT subscribes to its topics again.
- Each kind shows only the tabs it uses, and opening a request of another kind starts on its
  first tab instead of the one the last request was on. GraphQL opens on its Query;
  WebSocket and SSE have no Body (the handshake is a plain GET) and, like Socket.IO and
  MQTT, no Scripts, Vars or Asserts, which don't run on streams; gRPC has a JSON Message and
  Metadata but no Params (the URL is the server). Socket.IO's JSON body is its Auth payload.
  The Auth types offered are the ones the kind can send (MQTT: Basic only). A query left
  behind when a request turns from GraphQL into a stream or gRPC is dropped rather than sent.

## Authentication

| Type | Notes |
|---|---|
| Bearer, Basic, Digest | |
| API key | In a header or the query |
| AWS Signature v4 | Signed over the final URL, headers and body on every send; S3 and session tokens included; Postman's awsv4 both ways |
| OAuth 1.0a | HMAC-SHA1/256/512, RSA-SHA256/512, PLAINTEXT; signed over method, URL, query and form body; Postman and Insomnia import |
| JWT Bearer | HS/RS/PS/ES 256–512 and EdDSA, signed on each send from a JSON payload with variables; more header fields (`kid`…) as JSON; an HS secret given as Base64; sent after a prefix other than Bearer, or as a query parameter; Postman's jwt both ways |
| OAuth 2.0 | Client credentials, password, implicit, and authorization code with PKCE; the client ID and secret go in the token request's body or as a Basic header (Postman's and Insomnia's choice comes along on import) |
| Hawk | SHA-256 or SHA-1, a fresh nonce and timestamp per send, optionally the payload hash; Postman both ways, Insomnia import |
| Akamai EdgeGrid | Signed over the method, URL, chosen headers and a POST's body; Postman both ways |
| NTLM | NTLMv2 over HTTP/1.1, the challenge and answer on one connection; through a proxy only for https:// |

For authorization code and implicit, Send opens the browser to sign in and catches the
redirect on 127.0.0.1. An expired token is renewed with the refresh token when the provider
gives one, without signing in again. Tokens are kept in the workspace across restarts. The
Auth tab shows the current token and when it expires, with Copy and Forget (the next Send
then signs in from the start).

Secrets (passwords, client secrets, keys) are masked until the eye shows them; a value that
is a `{{variable}}` is shown and completed like any other field. An algorithm an import left
empty is the one Postman defaults to (HS256 for JWT).

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

The response's Tests tab is always there (before any test ran, it says where to add one) and
filters All / Passed / Failed, with counts.

## Responses

**Reading**

- JSON with colours, line numbers and folding of pretty objects and arrays (find opens a
  fold it lands in). Word wrap breaks between words.
- Pretty / Raw. XML responses are indented too; Raw, once chosen, sticks to that content type.
- Filter with JSONPath (`$.items[*].id`, `..id`, `$.items[?(@.price < 10 && @.tag == 'x')]`)
  or, on XML, XPath (`//item[@id='b']/name/text()`, prefixes ignored). The last 10 come back
  from Recent; Save… still writes the whole body.
- Find (Ctrl+F): match case, whole word or regular expression; counts up to 10 000 hits.
  The filter and the find stay on when the request is sent again.
- PNG, JPEG, GIF (its first frame) and WebP show as pictures, SVG too (Preview, beside Pretty and Raw), PDF page by
  page; other binary bodies show their size and "Save to file…". "Open in browser" shows an HTML, PDF or SVG one
  (a PDF in the system's viewer).

**Details**

- Hover the status code for what it means, the time for DNS lookup, TCP connect, TLS
  handshake, waiting (TTFB) and download, and the size for headers vs body.
- The Timeline tab: the request as it went out (headers the HTTP client adds and jar cookies
  included), each redirect, the peer address and where the time went (DNS, TCP, TLS, TTFB,
  download).
- The Cookies tab: what the response sets, as the jar keeps it: the host when there's no
  Domain, the URL's directory when there's no Path, Max-Age as a time, and the HttpOnly,
  Secure and SameSite flags. One for another site's domain is turned down and not listed.

**Keeping**

- Save… writes the body to a file (binary bodies byte for byte).
- Select text in the body and right-click it to set it as a variable, as in a request's text
  boxes.
- The History menu in the response bar: the last 10 responses of each request, each with its
  status, time and size, kept on disk with 1 MiB of body each. Request history and saved
  response examples too.
- The sidebar's History: each send with how long ago, filtered by name or URL; a click
  brings back what was sent.

**Big bodies**: text bodies past 5 MiB wait behind "Show anyway / Save to file…"; bodies
past 16 MiB keep only their start, to spare RAM. Send and download keeps a whole one.

## Collections, tabs and layout

**Tree**

- Filter by name. Each request's row shows its last status code.
- Drag a request or folder onto a folder (or the empty space under the tree) to move it,
  open tabs following, or onto a row's top or bottom edge to put it before or after that
  row. The order is kept in `.folder.toml`; Postman and Insomnia imports keep theirs.
- The ⋯ on a hovered row opens the same menu as right-click. A folder's starts with "Run
  collection" or "Run folder", a request's with "Open in new tab", which keeps the tab shown, a preview too.
- A new request goes into the folder last clicked in the tree, else beside the open request;
  its dialog can pick another folder.
- Save ⏷ "Save as…" saves the edits as a new request (`folder/name`); the original stays as
  saved.
- A `folder › request` breadcrumb, whose folders open their settings; a double-click on the
  request's name renames it.

**Tabs**: a click in the tree previews a request; a double-click, an edit or using it (Send,
Connect, a load test) keeps its tab. A
middle click closes one. Right-click a tab to duplicate it, or to close others, those to the
right or all, which leaves tabs with unsaved edits open. Quitting doesn't ask about unsaved
edits: the next start has them back in their tabs, still unsaved. They are also kept a couple
of seconds after each edit, so a crash or a session that ends loses no more than that.

**Layout**: the response sits beside the request; a window too narrow for two columns stacks
them until it's wider (the "Side by side" button greys out meanwhile). "Response below" and
"Side by side" in the status bar pick one place or the other, and "Sidebar" folds
the tree away; the sidebar can't be dragged narrower than its buttons. Stacked, the request
gets most of the height. The response's ⌄ (or `Ctrl+J`) folds it to a bar under the request
that still shows the last status, time and size; side by side, its › folds it to a strip at
the right with the status code. All three are kept across restarts, as are the
window's size, place and maximized. A screen too small for the default window gets it maximized.
With the sidebar folded, the environment picker moves to the right end of the tab strip.
Messages in the status bar fade after a few seconds; an error stays, in red at the bar's right
end, until its × dismisses it.

## Keyboard shortcuts

Ctrl is ⌘ on macOS, except for switching tabs. Until something is sent, the response pane
lists the everyday ones and `Ctrl+/` for the rest.

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
| Show or hide the response | `Ctrl+J` |
| Larger / smaller text | `Ctrl+=` / `Ctrl+-` |
| Default text size | `Ctrl+0` |
| Keyboard shortcuts | `Ctrl+/` |

Ctrl+K finds a request by folder and name (letters in order are enough) or by part of its URL, a folder (its
settings) or an environment, and does what a button does (new request, network settings,
cookies, side by side…). With nothing typed it lists the open tabs, then environments and
actions.

## Import and export

Import from the import icon above the tree (also ⋯ > "Import…", `Ctrl+O`, or "Import…" in an
empty tree): paste it, give its path or drop the file (or folder).

| Format | What comes in |
|---|---|
| curl | Paste it into the URL bar instead |
| Postman v2.1 | A collection, an environment or the globals |
| Postman v3 (Native Git, the Postman CLI) | A collection's folder of YAML, or a whole workspace folder with its collections, environments and globals ("Choose folder…"); also one v3 document. HTTP, GraphQL, gRPC, WebSocket, Socket.IO and MQTT requests, with folder auth, variables, scripts, examples and saved messages. Globals already set keep their value; MCP and LLM requests, specs, flows and mocks are listed as left out |
| OpenAPI 3 / Swagger 2, JSON or YAML | A request per operation, a folder per tag, `{{baseUrl}}` from the server, auth from the security schemes, bodies and saved examples from the schemas, so the mock server answers right away |
| Insomnia v4 JSON / v5 YAML | Folders, auth and scripts; `{{ _.x }}` as `{{x}}`; the base environment as collection variables and each sub-environment as an environment |
| HAR from browser devtools | A folder per host, each response kept as the request's example |

Right-click a folder or collection > "Copy as" or "Export" for every format the other clients
import:

| Format | What it holds | Imported by |
|---|---|---|
| Postman collection v2.1 | Folders, auth, scripts, variables, examples; not WebSocket, SSE or gRPC | Postman, Hoppscotch, Insomnia, Bruno, Yaak, Restfox, Yaade |
| Insomnia v4 | Folders with their variables, auth and scripts, WebSocket too; the collection's variables as the base environment, its auth and scripts on what's directly in it | Insomnia, Hoppscotch, Bruno, Yaak, Restfox |
| OpenAPI 3.1, Markdown | API docs, see below | |
| HAR 1.2 | Each request as sent, variables filled in, with its first saved example as the answer | Hoppscotch, Insomnia, browser devtools |
| cURL | Each request as sent, a command under a comment naming it | Every client's cURL import, a shell |

An environment (from its editor) copies as a Postman environment, its secrets as Postman's
secret type.

"</> Code" writes the request as curl, wget, HTTPie, PowerShell, raw HTTP, Python, fetch,
axios, Go, Java, C#, PHP, Ruby, Rust, Swift or Kotlin. A Binary body reads its file, and
`--data-binary @file` pastes back as one. Streams get their own clients; the language picked
last carries over between kinds. Long lines scroll sideways rather than wrap.

| Kind | Clients |
|---|---|
| SSE | curl, fetch, requests, Go |
| WebSocket | websocat, browser, ws, websockets, gorilla |
| Socket.IO | socket.io-client, python-socketio |
| MQTT | mosquitto, paho-mqtt, mqtt.js, paho Go |
| gRPC | grpcurl, grpc-js, grpcio, by the method's streaming shape |

Descriptions are Markdown, written in a request's or folder's Docs tab; Preview there shows
them laid out, and a link in them opens in the browser.

API docs come from request and folder descriptions and saved examples: right-click a
folder (or More above the collections) > "Copy as" or "Export" > Markdown or OpenAPI, or
`apitool-cli docs`. The OpenAPI 3.1 document (YAML,
or JSON for a .json file) imports back as the same requests: the server from `{{baseUrl}}`
with the folder's value, `:id` and `{{var}}` path segments as path parameters, query
parameters that are off as optional, a tag per subfolder, bodies and saved examples with a
schema read off them, and auth as its security scheme, never a credential. Streams, gRPC
and a second request to a method and path already taken are left out, and counted.

## Runner, load test and mock server

- **Collection runner** with CSV or JSON data. It opens as a "Run:" chip at the end of the
  tab strip; opening a request keeps the run going there, and the chip's × stops it and
  closes the results. Counts cover every row; the list keeps
  failures and the latest 1000 results. Tick the requests to run and drag them into the
  order wanted (kept per folder on this machine; ↺ goes back to the tree's). "Stop at the
  first failure" ends the run there. The export icon writes the results as JUnit XML.
  Options as in Postman: keep the variables a run sets (off, the environment and globals
  stay as they were), start from an empty cookie jar, save the cookies after the run, and
  keep each response (32 KiB of body) to open from the results. Run again repeats a shown
  run with its requests, order and options.
- **Load test** pane. Response bodies are read and dropped, so many users cost no RAM.
- **Mock server** answering with saved examples: right-click a folder > "Start mock server",
  or `apitool-cli mock`. An `x-mock-response-name` or `x-mock-response-code` header picks
  the example.

## Capture

"Capture" in the status bar (or "Capture traffic" from Ctrl+K) opens a pane
like Proxyman's or Charles's. Start capturing, then set the address shown, `127.0.0.1:9090`
(the port is set next to Start and kept for the workspace; when it's taken, another free one,
and the status bar says so), as the HTTP or SOCKS (4, 4a, 5, without a login) proxy of an
app, both on the one port, or tick "Use as the system proxy" to point the system's HTTP and
HTTPS proxy there (Windows' Internet Options, macOS's network services; a proxy script or
auto-detection is turned off meanwhile); the status bar then reads "Capturing on …, as the
system proxy". apitool's own requests go through it then too, when its proxy setting is
System; the capture itself keeps forwarding the way the system's proxy was before. Stop,
closing the capture or quitting puts back what was set; after a crash, the next start does.
An app that read the system's proxy when it started (Telegram, a shell's `HTTPS_PROXY`) may
need a restart to see it; curl doesn't read it at all.
It listens on 127.0.0.1 only unless "Other devices too" is ticked before Start: then on
every interface, so WSL, a VM or a phone can use it at this machine's network address
(shown; from WSL 2, the Windows host's address, the default gateway, or 127.0.0.1 with
mirrored networking). Anyone who can reach the machine can then send through it, and
Windows may ask whether apitool may accept connections. What it sends goes on with apitool's own network settings (its proxy, CA and client
certificates) and shows as a row: time, method, status, duration, size, URL. A row opens
underneath with both sides' headers and bodies; "Open as request" saves it as a new
request, its answer as an example. The export icon writes every flow as HAR. HTTPS passes
through as a tunnel, shown with its host and the bytes each way, unless "Decrypt HTTPS" is
on: then each host gets a certificate from apitool's own CA, made once and kept in the
workspace (its key sealed as secrets are), and its requests show as any other. Each
certificate names a CRL the capture itself serves (signed by the CA, revoking nothing), as
Windows' TLS in curl, git and .NET refuses a certificate whose revocation it can't check
(`CRYPT_E_NO_REVOCATION_CHECK`). Inside a SOCKS tunnel, plain HTTP is read as flows too; what's
neither HTTP nor TLS (Telegram's own protocol, say) goes through as a tunnel. "Export CA
certificate…" saves the CA as a .crt file for the system's trusted roots or an app's; an app
that doesn't trust it fails to connect, and its tunnel says so. "Trusting apitool's CA",
shown while decrypting, has the steps for macOS, Windows, Linux and WSL, Firefox, command-line
tools (Node, Python, curl, Java) and phones, each with how to take it back, its commands
naming the exported file, a click to copy each. Decrypted
connections speak HTTP/2 when the app offers it, so gRPC goes through, its trailers too, and
so does gRPC to an http:// server (h2c). WebSockets (and Socket.IO) go through, a flow each
with the bytes sent and received. MQTT (3.1, 3.1.1 and 5) through it is a flow listed as MQTT,
with each packet both ways: a CONNECT's client ID, login and will, each PUBLISH's topic, QoS and
message (shown as its MQTT 5 content type says), what each subscription asked for and was
granted, acknowledgements and reason codes by name, and MQTT 5's properties (a topic sent as an
alias shows as the topic). mqtts reads the same while decrypting, going on to the broker over
TLS with apitool's network settings, and so does MQTT over WebSocket (the `mqtt` subprotocol);
ws:// in a CONNECT tunnel needs "Decrypt HTTPS" on to be read, as plain HTTP there does. A gzip, deflate or br body shows unpacked. The latest
1000 flows are kept, each body's first 256 KiB.

Rules, as Proxyman's tools, apply to every exchange whose URL matches (anywhere in the URL,
`*` for any part), top to bottom, and are kept in the workspace: **Map Local** answers from a
file and never reaches the server; **Map Remote** sends to another server (its scheme, host
and port; the path and query stay); **Request header** and **Response header** set one, or
remove it when the value is empty; **Throttle** holds the request, then lets the answer
through at most so many KiB/s; **Breakpoint** holds the request before it goes on, the
answer before the app gets it (a Map Local file's too), or both. A held exchange shows atop the pane as text (its
first line, a header a line, a blank line, the body): edit it, then **Continue** sends what
it says, or **Abort** answers the app with a 502. A binary body goes on as it was unless its
note is typed over; Stop drops whatever is held. A row a rule changed has an orange dot, and
its detail says what changed. HTTPS needs "Decrypt HTTPS" on for rules to see inside.

**Connections**, beside Flows, lists every TCP and UDP connection the OS has open, as
Sniffnet does, without the proxy: each process (name and PID), its local and remote
address, and the state, listed again every 2 seconds and filtered by process, address or
state. Listeners are dimmed. It reads the OS's own tables (`lsof` on macOS and Linux,
`netstat` and `tasklist` on Windows) rather than capturing packets, which would need a
driver installed (Npcap), so there are no byte counts. On macOS and Linux only your own
processes show; the others' need root.

## Command line

```
apitool-cli <folder|request> [options]   run requests and their tests
apitool-cli docs [folder] [-o api.md]     Markdown API docs (default: stdout);
                                          -o api.yaml or api.json writes OpenAPI 3.1
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
| Log, `apitool.log`; the window, `.window.ron` | `$XDG_STATE_HOME/apitool` (`~/.local/state/apitool`) | The workspace |

The log keeps 1 MB and the one before it (`apitool.log.1`). Each line says which source line
wrote it (`src/http.rs:478`). Settings > General sets how much goes in: Info by default;
Debug adds each step of a request or stream (the proxy or PAC answer used, the connection,
TLS, redirects, status and timings) and the network libraries' own steps; Trace also each
message's size and a response's header names. Headers' values, bodies and tokens are never
written, and every URL loses its query. The folder icon beside it opens the log's folder.

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

## Credits

Interface icons are from [Lucide](https://lucide.dev) (ISC licence, in
`assets/icons/LICENSE`).
