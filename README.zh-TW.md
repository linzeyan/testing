# apitool

[English](README.md) | 正體中文

輕巧、免安裝的 Postman 風格 API 用戶端，以 Rust 與 egui 打造，專為 Citrix VDI 這類記憶體吃緊的
環境設計。解壓縮就能執行。

- **GUI、CLI、MCP 都有**：`apitool` 視窗、給 CI 用的 `apitool-cli`，以及給 LLM 用戶端的 MCP 伺服器。
- **相容 Postman**：可匯入 Postman、OpenAPI、Insomnia 與 HAR，並執行 `pm.*` 指令碼。
- **省記憶體**：大型內容會截斷或直接串流寫入磁碟；負載測試不保留回應內容。
- **適合 git**：集合以純文字 TOML 儲存；可選擇透過 GitHub 或 GitLab 儲存庫同步。

## 安裝

從 [Releases](https://github.com/linzeyan/testing/releases/latest) 下載對應系統的壓縮檔，解壓到
任何位置後執行 `apitool`。

| 系統 | 壓縮檔 |
|---|---|
| Windows x64 | `apitool-v<version>-x86_64-pc-windows-msvc.zip` |
| macOS（Apple 晶片） | `apitool-v<version>-aarch64-apple-darwin.tar.gz` |
| macOS（Intel） | `apitool-v<version>-x86_64-apple-darwin.tar.gz` |
| Linux x64 | `apitool-v<version>-x86_64-unknown-linux-gnu.tar.gz` |

每個壓縮檔都包含 `apitool`（GUI）與 `apitool-cli`（執行器、文件、模擬伺服器與 MCP 伺服器）。
apitool 預設每天檢查一次新版本，找到時會提供原地安裝，下次啟動生效；可在「設定 > 更新」調整
檢查頻率（從不、每次啟動、每天、每週），以及是否不經詢問直接安裝。介面支援英文與正體中文
（「設定 > 一般」）。

以 stable Rust 從原始碼建置：

```sh
cargo build --release --locked   # target/release/apitool and apitool-cli
```

## 通訊協定

| 協定 | 說明 |
|---|---|
| HTTP | 每個請求可個別設定 HTTP 版本、重新導向、TLS 驗證、Cookie 與逾時 |
| GraphQL | `subscription` 透過 WebSocket 連線（graphql-transport-ws 或較舊的 graphql-ws），逐筆串流結果 |
| WebSocket | |
| SSE | |
| Socket.IO 4 | URL 的路徑即 namespace，JSON 內容即 auth payload；以 `event {"json": "arg"}` 或 `["event", …]` 陣列送出；會顯示 ack |
| MQTT 3.1.1 與 5 | 透過 TCP、TLS 或 WebSocket（`mqtt://`、`mqtts://`、`ws://`、`wss://`），詳見下方 |
| gRPC | 方法來自執行時載入的 `.proto`；未指定時改用伺服器反射（v1 或 v1alpha，按 ↻ 或第一次傳送時查詢）。串流方法像 WebSocket 一樣連線；JSON 陣列內容會送出多則訊息 |

MQTT：

- 按「連線」時訂閱請求裡的主題，連線期間修改主題也會跟著調整訂閱。
- 從訊息面板發布，可設定 QoS 與保留（retain）。MQTT 5 的使用者屬性可隨訊息送出，收到的訊息
  也會顯示。
- 可設定遺囑訊息（主題、內容、QoS、保留），隨連線一起送出。
- TLS 沿用網路設定中的 CA 檔、用戶端憑證與「接受任何憑證」選項；連線走 https 所用的同一個
  代理伺服器，以 CONNECT 建立通道。

## 請求

**內容**

- JSON；文字（選 Text、XML、HTML 或 JavaScript 會設定對應的 Content-Type；XML 可美化）；表單；
  multipart（值寫 `@path` 即上傳該檔案）。
- 二進位檔：原樣送出檔案，從磁碟串流讀取，依副檔名判斷類型。
- 超過 128 KB 的文字（在內容、指令碼、範例或 Postman 匯入框中）會保留並照常送出，但不會排版
  供編輯：曾有 10 MB 的內容光是顯示就用掉 3.6 GB 記憶體。

**參數、標頭與變數**

- 每個參數、標頭與表單欄位都能加說明；「批次編輯」以 `key: value` 逐行編輯這些表格。
- 路徑變數：`/users/:id` 會在「參數」下產生一列 `id`，傳送時代入；Postman 匯入與匯出都會保留。
- 輸入時自動完成標頭名稱與 Content-Type 值。「標頭」分頁可展開查看傳送時自動加上的標頭（Host、
  驗證、Content-Type…）及各自的來源。
- 支援 Postman 全部 120 個動態變數（`{{$guid}}`、`{{$timestamp}}`、`{{$randomEmail}}`、
  `{{$randomBankAccountIban}}`…），每次使用都產生新值；輸入 `{{email` 會自動完成為
  `$randomEmail`。
- `{{?name}}` 提示變數：傳送時詢問，只保留到 apitool 關閉；Insomnia 的 `{% prompt %}` 會匯入成
  這種變數。
- 環境可以設定顏色（例如 prod 設紅色），顯示在環境選單上，並在視窗頂端加一條色帶。
- 資料夾設定（在資料夾上按右鍵）可設定變數、驗證與指令碼，由資料夾內所有請求共用，相當於
  Postman 的集合與資料夾設定。

**網路**

- 代理伺服器：http、https 與 SOCKS（`socks5h://host:1080`）、PAC（含 `SOCKS`/`SOCKS5` 項目）
  與 WPAD。
- 自訂 CA；用戶端憑證支援 PEM 或 PFX/P12，可設一個預設憑證，再依主機個別指定（`api.corp.com`、
  `*.corp.com`、`host:8443`）。
- 和瀏覽器一樣的 Cookie jar，附管理介面。

**傳送**

- 在 URL 列貼上 curl 指令即可匯入。
- 「傳送 ⏷ > 傳送並下載…」把成功的回應直接串流寫入檔案，再大都可以；失敗或取消時不會留下
  不完整的檔案。
- 「傳送 ⏷ > 每隔一段時間重送」依設定的秒數重複傳送，同一時間只會有一個請求在跑，直到按
  「停止重送」、取消或切換到其他分頁為止。

## 驗證

| 類型 | 說明 |
|---|---|
| Bearer、Basic、Digest | |
| API 金鑰 | 放在標頭或查詢字串 |
| AWS Signature v4 | 每次傳送都對最終的 URL、標頭與內容簽章；支援 S3 與 session token；與 Postman 的 awsv4 雙向互通 |
| OAuth 1.0a | HMAC-SHA1/256/512、RSA-SHA256/512、PLAINTEXT；對方法、URL、查詢字串與表單內容簽章；可從 Postman 與 Insomnia 匯入 |
| JWT Bearer | HS/RS/PS/ES 256–512 與 EdDSA；每次傳送時以含變數的 JSON payload 簽發；與 Postman 的 jwt 雙向互通 |
| OAuth 2.0 | Client credentials、password、implicit，以及搭配 PKCE 的 authorization code |

使用 authorization code 與 implicit 時，按傳送會開啟瀏覽器登入，並在 127.0.0.1 接收重新導向。
token 過期時，若提供者有給 refresh token 就自動換新，不必重新登入。token 存在工作區中，重新
啟動後仍然有效。

## 指令碼與測試

Postman 風格的請求前與測試指令碼，預設是空的，由你自己撰寫（「插入範本」與「程式碼片段」可以
幫你填入一些）：

- `pm.*`、chai、`jsonSchema`、`pm.variables.replaceIn`。
- 可 `require` Postman 沙箱的模組（crypto-js、lodash、moment、uuid、tv4、chai），另有
  `atob`/`btoa`；每個模組在被 require 時才載入。
- `pm.cookies` 與 `pm.cookies.jar()` 讀寫傳送時所用的同一個 Cookie jar。
- `pm.sendRequest` 支援 callback 或 `await`，走同一套用戶端，例如先取得 token。
- 在執行器中，`pm.execution.setNextRequest(name)`（或 `postman.setNextRequest`）可跳轉或迴圈，
  傳入 `null` 則停止；`pm.execution.skipRequest()` 會跳過這個請求。

不寫 JavaScript：

- 「斷言」分頁，作法同 Bruno：例如 `res.status` `eq 200`、`res.body.items` `length 3`、
  `res.headers['content-type']` `contains json`。共 28 種運算子，右側可用 `{{variables}}`；
  每一列就是一筆測試結果。
- 回應後「程式碼片段」選單中的「回應符合 schema」，會依回應產生 `jsonSchema` 契約測試（陣列
  元素合併；只有部分元素才有的鍵設為選填）。

「測試」分頁可依「全部 / 通過 / 失敗」篩選，並顯示各自的數量。

## 回應

**閱讀**

- JSON 有語法上色與行號，美化後的物件與陣列可以摺疊（搜尋命中摺疊區時會自動展開）。自動換行
  會在字詞之間斷行。
- 「美化 / 原始」：XML 回應也會縮排；選了「原始」之後，同一種 Content-Type 會記住這個選擇。
- 以 JSONPath 篩選（`$.items[*].id`、`..id`、`$.items[?(@.price < 10 && @.tag == 'x')]`），
  XML 則用 XPath（`//item[@id='b']/name/text()`，忽略命名空間前綴）。最近用過的 10 筆可從
  「最近」叫回；「儲存…」仍會寫出完整內容。
- 尋找（Ctrl+F）：可選大小寫須相符、全字相符或規則運算式；最多計數到 10 000 筆。
- PNG、JPEG、WebP 直接顯示為圖片，SVG 也可以（「預覽」，位於「美化」與「原始」旁），PDF 逐頁
  顯示；其他二進位內容只顯示大小。「在瀏覽器開啟」可開啟 HTML、PDF 或 SVG（PDF 用系統的檢視器）。

**細節**

- 滑鼠停在狀態碼上會說明其意義；停在耗時上會拆解 DNS 查詢、TCP 連線、TLS 交握、等待（TTFB）
  與下載；停在大小上會分列標頭與內容。
- 「時間軸」分頁：實際送出的請求（含 HTTP 用戶端自動加上的標頭與 jar 中的 Cookie）、每一次
  重新導向，以及對端位址。
- 「Cookie」分頁：回應設定了哪些 Cookie。

**保存**

- 「儲存…」把內容寫入檔案（二進位內容逐位元組原樣寫出）。
- 回應列的「歷史紀錄」選單：每個請求最近 10 筆回應，存在磁碟上，每筆最多保留 1 MiB 內容。
  另有請求歷史紀錄與已儲存的回應範例。

**大型內容**：超過 5 MiB 的文字內容會先停在「仍然顯示 / 儲存到檔案…」；超過 16 MiB 的內容只
保留開頭，以節省記憶體。要完整保存請用「傳送並下載…」。

## 集合、分頁與版面

**樹狀清單**

- 可依名稱篩選。每個請求那一列會顯示最後一次的狀態碼。
- 把請求或資料夾拖到某個資料夾上（或樹狀清單下方的空白處）即可移動，已開啟的分頁會跟著走；
  拖到某一列的上緣或下緣，則放到該列之前或之後。順序記錄在 `.folder.toml`；從 Postman 與
  Insomnia 匯入時保留原本的順序。
- 滑鼠移到某一列時出現的 ⋯，開啟的選單和按右鍵相同。
- 新請求會放進樹狀清單中最後點選的資料夾，否則放在目前開啟的請求旁邊；建立對話框中可以改選
  其他資料夾。
- 「儲存 ⏷ > 另存為…」把修改存成新請求（`folder/name`），原請求維持已儲存的內容。
- `folder › request` 麵包屑導覽，點其中的資料夾可開啟其設定。

**分頁**：在樹狀清單點一下是預覽；按兩下或開始編輯，分頁就會固定下來。中鍵點擊可關閉分頁。
在分頁上按右鍵可複製一份，或關閉其他分頁、右側分頁或所有分頁，有未儲存變更的分頁會保持開啟。

**版面**：回應預設在請求右側；視窗寬度不夠排兩欄時改為上下堆疊，拉寬後恢復。狀態列的「左右
並排」可關閉這個行為，「側邊欄」可收起樹狀清單；兩者在重新啟動後都會保留。

## 鍵盤快捷鍵

macOS 上 Ctrl 換成 ⌘（切換分頁除外）。尚未傳送任何請求前，回應窗格也會列出這些快捷鍵。

| 動作 | 按鍵 |
|---|---|
| 傳送請求 | `Ctrl+Enter` |
| 傳送並下載… | `Ctrl+Shift+Enter` |
| 儲存變更 | `Ctrl+S` |
| 另存為… | `Ctrl+Shift+S` |
| 前往請求、資料夾或動作 | `Ctrl+K` |
| 新增請求 | `Ctrl+N` |
| 新增集合 | `Ctrl+Shift+N` |
| 匯入… | `Ctrl+O` |
| 複製一份請求或資料夾 | `Ctrl+D` |
| 重新命名樹狀清單中點選的資料夾，否則重新命名開啟中的請求 | `F2` |
| 刪除樹狀清單中選取或點選的項目 | `Delete`（macOS 為 `⌘⌫`） |
| 選取 URL | `Ctrl+L` |
| 程式碼 | `Ctrl+Shift+G` |
| 編輯環境 | `Ctrl+E` |
| 在回應中尋找 | `Ctrl+F` |
| 關閉分頁 | `Ctrl+W` |
| 重新開啟關閉的分頁（回到原位置） | `Ctrl+Shift+T` |
| 下一個 / 上一個分頁 | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 跳到第 1–8 個分頁、最後一個 | `Ctrl+1`…`9` |
| 側邊欄 | `Ctrl+\` |
| 左右並排 | `Ctrl+Alt+V` |

Ctrl+K 可依資料夾與名稱找請求（依序打出其中幾個字母即可）、找資料夾（開啟其設定）或環境，也能
執行按鈕的功能（新增請求、網路設定、Cookie、左右並排…）。

## 匯入與匯出

從「⋯ > 匯入…」匯入：貼上內容、輸入路徑，或直接把檔案拖進來。

| 格式 | 匯入內容 |
|---|---|
| curl | 改為直接貼到 URL 列 |
| Postman v2.1 | 集合或環境 |
| OpenAPI 3 / Swagger 2（JSON 或 YAML） | 每個 operation 一個請求、每個 tag 一個資料夾；`{{baseUrl}}` 取自 server，驗證取自 security scheme，內容與已儲存範例取自 schema，因此匯入後模擬伺服器馬上就能回應 |
| Insomnia v4 JSON / v5 YAML | 資料夾、驗證與指令碼；`{{ _.x }}` 轉為 `{{x}}`；base environment 成為集合變數，每個 sub-environment 成為一個環境 |
| 瀏覽器開發者工具的 HAR | 每個主機一個資料夾，每筆回應保留為該請求的範例 |

資料夾或整個集合可以「複製為 Postman 集合」。

「</> 程式碼」可將請求轉成 curl、wget、HTTPie、PowerShell、原始 HTTP、Python、fetch、axios、Go、
Java、C#、PHP、Ruby、Rust、Swift 或 Kotlin。二進位內容會讀入對應的檔案，而 `--data-binary @file`
貼回 URL 列時也會還原成二進位內容。串流類請求會產生各自用戶端的程式碼；上次選的語言會在不同
類型的請求之間沿用。

| 類型 | 用戶端 |
|---|---|
| SSE | curl、fetch、requests、Go |
| WebSocket | websocat、瀏覽器、ws、websockets、gorilla |
| Socket.IO | socket.io-client、python-socketio |
| MQTT | mosquitto、paho-mqtt、mqtt.js、paho Go |
| gRPC | grpcurl、grpc-js、grpcio，依方法的串流形式產生 |

Markdown API 文件由請求與資料夾的說明及已儲存的範例產生：在資料夾上按右鍵 >「複製文件為
Markdown」，或執行 `apitool-cli docs`。

## 執行器、負載測試與模擬伺服器

- **集合執行器**：可搭配 CSV 或 JSON 資料。統計涵蓋每一列資料；結果清單保留所有失敗項目與最近
  1000 筆結果。
- **負載測試**窗格：回應內容讀取後即丟棄，因此模擬的使用者再多也不佔記憶體。
- **模擬伺服器**：以已儲存的範例回應；在資料夾上按右鍵 >「啟動模擬伺服器」，或執行
  `apitool-cli mock`。請求帶上 `x-mock-response-name` 或 `x-mock-response-code` 標頭，可指定
  要回應哪個範例。

## 命令列

```
apitool-cli <folder|request> [options]   執行請求與其測試
apitool-cli docs [folder] [-o api.md]     產生 Markdown API 文件（預設輸出到 stdout）
apitool-cli mock [folder] [--port 3000]   在 localhost 啟動模擬伺服器
apitool-cli mcp [--window]                以 stdio 提供 MCP 伺服器
```

所有指令都接受 `--workspace <dir>`。請求名稱與樹狀清單中的相同（`users`、`users/get user`；`.`
代表整個集合）。

| 選項 | |
|---|---|
| `-e, --env <name>` | 使用的環境 |
| `-d, --data <file>` | CSV 或 JSON 資料檔；每一列跑一輪 |
| `-n, --iterations <n>` | 沒有資料檔時的執行輪數（預設 1） |
| `--delay <ms>` | 請求之間的間隔 |
| `--junit <file>` | 另外輸出 JUnit XML 報告 |

結束碼：`0` 全部通過、`1` 有失敗、`2` 無法執行。

## MCP 伺服器

`apitool-cli mcp` 讓 LLM 用戶端在工作區中列出、讀取、寫入、移動、刪除、傳送與執行請求，編輯
資料夾與變數，匯入集合與規格，產生程式碼，讀取歷史紀錄，以及監聽串流（SSE、WebSocket、
Socket.IO、MQTT、gRPC）。機密值除非明確要求，否則一律遮蔽，程式碼片段中也一樣。

「設定 > MCP」提供 Claude Code、Claude Desktop 或 Cursor 可直接複製的設定：

```json
{ "mcpServers": { "apitool": {
    "command": "C:\\tools\\apitool\\apitool-cli.exe",
    "args": ["mcp", "--workspace", "C:\\tools\\apitool\\workspace"] } } }
```

視窗重新取得焦點時，GUI 會重新讀取工作區。

開啟「讓它操作這個視窗」後，視窗本身會提供 MCP 服務（每次啟動產生新的 token，拒絕來自網頁的
連線），`apitool-cli mcp --window` 則轉接到它。預設監聽 127.0.0.1，每次啟動挑一個新的空閒連接
埠，除非在設定中指定了主機與連接埠。直接用 HTTP 連線的用戶端，則送到設定中顯示的網址，並把同
樣顯示在那裡的 token（旁邊有複製圖示）放在 `Authorization: Bearer`，走 HTTP/1.1 或不加密的
HTTP/2（h2c，prior knowledge）皆可。此時用戶端還能看到目前開著什麼（未儲存的修改、回應、進行
中的串流），開啟請求、按下傳送、切換環境，所做的變更也會立即顯示。

## 工作區與資料

| | macOS 與 Linux | Windows |
|---|---|---|
| 工作區 | `$APITOOL_WORKSPACE`，未設定時為 `$XDG_DATA_HOME/apitool`（`~/.local/share/apitool`） | `$APITOOL_WORKSPACE`，未設定時為執行檔旁的 `workspace/` |
| 記錄檔 `apitool.log` | `$XDG_STATE_HOME/apitool`（`~/.local/state/apitool`） | 工作區內 |

工作區把所有資料存在單一 SQLite 檔 `apitool.db`。集合、環境與全域變數另外以 TOML 寫在旁邊，
方便納入 git（`collections/`、`environments/`、`globals.toml`），雙向自動保持一致，不需要任何
操作。

機密值、歷史紀錄、Cookie 與 OAuth token 只存在資料庫中。機密值由作業系統加密（Windows 用
DPAPI、macOS 用登入鑰匙圈中的金鑰；Linux 上不加密），因此資料庫複製給其他使用者或其他電腦後
無法解開。

## 同步

「設定 > 同步」把工作區的檔案樹放進 GitHub.com 或 GitLab.com 的私有儲存庫，讓兩台電腦共用同一個
工作區。

1. 填入儲存庫（`owner/name` 或網址）、分支與 token；token 和機密值一樣加密保存。「建立 token」
   會開啟提供者的頁面，並預先填好所需設定：
   - GitHub：fine-grained token，對該儲存庫有 Contents 讀寫權限。
   - GitLab：`api` scope。
2. 按狀態列的 ↻，會拉下遠端的變更、推上本機的變更，合成一個 commit；連線走請求所設定的代理
   伺服器。設定「自動同步」後，apitool 啟動時也會同步一次，之後每 5、15、30 或 60 分鐘一次。

上次同步後兩邊都改過的請求，會詢問要保留儲存庫的版本還是這台電腦的版本。

| 資料 | 是否同步 |
|---|---|
| 集合、環境、全域變數 | 一律同步 |
| 機密值、歷史紀錄、Cookie、OAuth token | 只同步在「一併同步」中勾選的項目 |
| 過去的回應內容、介面狀態 | 不同步 |

勾選的項目會以密語加密（每台電腦使用相同密語；PBKDF2 與 AES-256-GCM），存進儲存庫的
`.apitool/`，絕不會寫進工作區資料夾；並逐筆合併：兩台電腦的傳送紀錄會累加，兩邊都改過的機密值
會詢問，兩邊都改過的 Cookie 與 token 則各自保留本機的版本。

儲存庫中的其他檔案不會被動到。
