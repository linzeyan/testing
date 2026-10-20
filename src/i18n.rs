//! The interface in English or Traditional Chinese (Taiwan). Text is written in English
//! in the code and wrapped in `t()`, which looks it up here.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    #[default]
    English,
    ZhTw,
}

thread_local! {
    // Per thread, not per process: the UI thread sets it each frame, and tests that run
    // in parallel threads each keep their own.
    static LANG: Cell<Lang> = const { Cell::new(Lang::English) };
}

pub fn set(lang: Lang) {
    LANG.set(lang);
}

/// `en` in the current language; English when there's no translation.
pub fn t(en: &'static str) -> &'static str {
    match LANG.get() {
        Lang::English => en,
        Lang::ZhTw => ZH_TW_MAP.get(en).copied().unwrap_or(en),
    }
}

/// `t(template)` with each `{}` filled in from `args`, in order.
pub fn tf(template: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    fill(t(template), args)
}

/// `text`'s `{}` filled in from `args`, in order. For text built off the UI thread: other
/// threads read English, so the template is translated with `t` before handing it over.
pub fn fill(text: &str, args: &[&dyn std::fmt::Display]) -> String {
    use std::fmt::Write as _;
    let mut parts = text.split("{}");
    let mut out = parts.next().unwrap_or_default().to_owned();
    for (part, arg) in parts.zip(args.iter().map(Some).chain(std::iter::repeat(None))) {
        if let Some(arg) = arg {
            let _ = write!(out, "{arg}");
        }
        out.push_str(part);
    }
    out
}

/// Marks text for translation where `t` can't run (a const); `t` translates it where
/// it's shown.
pub const fn n_(en: &'static str) -> &'static str {
    en
}

static ZH_TW_MAP: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| ZH_TW.iter().copied().collect());

const ZH_TW: &[(&str, &str)] = &[
    ("  · bidi stream", "  · 雙向串流"),
    ("  · client stream", "  · 用戶端串流"),
    ("  · server stream", "  · 伺服器串流"),
    (" and 1 environment", "，以及 1 個環境"),
    (" and {} environments", "，以及 {} 個環境"),
    (" and {} globals", "，以及 {} 個全域變數"),
    ("(not set)", "(未設定)"),
    ("(top level)", "（最上層）"),
    ("*.corp.com or host:8443", "*.corp.com 或 host:8443"),
    (
        "+ and # are for subscribing; publish to one topic",
        "+ 和 # 只用於訂閱；發布時請指定單一主題",
    ),
    ("Certificate for a host", "為主機加入憑證"),
    ("File…", "檔案…"),
    ("Topic", "主題"),
    (", the redirect included", "，含重新導向"),
    (", {} redirects included", "，含 {} 次重新導向"),
    (
        "// Runs after the response arrives.\n// pm.test(name, fn), pm.expect(...), pm.response.json()",
        "// 收到回應後執行。\n// pm.test(name, fn), pm.expect(...), pm.response.json()",
    ),
    (
        "// Runs before the request is sent.\n// pm.request, pm.environment, pm.variables, console.log",
        "// 送出請求前執行。\n// pm.request, pm.environment, pm.variables, console.log",
    ),
    ("0 sends no pings", "0 表示不送 ping"),
    (
        "0 uses the network settings' timeout",
        "0 表示沿用網路設定的逾時",
    ),
    (
        "0: at most once · 1: at least once · 2: exactly once",
        "0：最多一次 · 1：至少一次 · 2：恰好一次",
    ),
    (
        "1 tab with unsaved edits left open",
        "1 個分頁有未儲存的變更，保持開啟",
    ),
    (
        "5.0 says why a broker refuses or hangs up, and has user properties",
        "5.0 會說明 broker 拒絕或斷線的原因，並支援使用者屬性",
    ),
    (
        "; left out {} WebSocket/SSE/gRPC, which collections can't hold",
        "；略過 {} 個 WebSocket/SSE/gRPC，集合無法容納",
    ),
    ("\"{}\" already exists", "「{}」已存在"),
    ("\"{}\" already exists there", "那裡已有「{}」"),
    ("\"{}\" no longer exists", "「{}」已不存在"),
    (
        "A Postman v3 collection folder, or a Native Git workspace with its environments and globals",
        "Postman v3 的集合資料夾，或含環境與全域變數的 Native Git 工作區",
    ),
    (
        "A broker drops the older of two connections with the same ID",
        "同一個 ID 有兩條連線時，broker 會斷開較舊的那條",
    ),
    ("A folder can't go inside itself", "資料夾不能放進自己裡面"),
    (
        "a folder name can't end with .toml",
        "資料夾名稱不能以 .toml 結尾",
    ),
    (
        "A Postman collection, environment or globals (v2.1, or a v3 folder), an OpenAPI 3 / Swagger 2 spec, an Insomnia v4/v5 export or a HAR from the browser's devtools: choose the file or folder, paste it or its path, or drop it here. Nothing already here is replaced. A curl command goes into the URL bar instead.",
        "Postman 集合、環境或全域變數（v2.1，或 v3 資料夾）、OpenAPI 3 / Swagger 2 規格、Insomnia v4/v5 匯出檔，或瀏覽器開發者工具的 HAR：選擇檔案或資料夾、貼上內容或路徑，或拖放到這裡。不會取代既有的任何項目。curl 指令請直接貼到網址列。",
    ),
    (
        "A Postman collection or environment, or an OpenAPI/Swagger spec",
        "Postman 集合或環境，或 OpenAPI/Swagger 規格",
    ),
    (
        "A successful response goes straight to this file, however big.",
        "成功的回應不論多大，都直接寫入這個檔案。",
    ),
    (
        "A value starting with @ uploads that file, e.g. @files/photo.png (relative to the workspace). Or drop files here.",
        "以 @ 開頭的值會上傳該檔案，例如 @files/photo.png（相對於工作區）。也可以把檔案拖到這裡。",
    ),
    ("Aa", "Aa"),
    (
        "Accepted: received, but not acted on yet.",
        "Accepted：已收到，但尚未處理。",
    ),
    ("Access key", "存取金鑰"),
    ("Access token", "存取權杖"),
    ("Add a file part", "加入檔案欄位"),
    ("Add to", "加入到"),
    ("Add to the query", "加入查詢"),
    ("Algorithm", "演算法"),
    ("All ({})", "全部 ({})"),
    ("All collections", "所有集合"),
    ("all collections", "所有集合"),
    (
        "Always available: $guid, $timestamp, $randomInt, $randomEmail… ({} in all; type {{$ for the list)",
        "隨時可用：$guid、$timestamp、$randomInt、$randomEmail…（共 {} 個；輸入 {{$ 可看清單）",
    ),
    (
        "An event and its argument: chat {\"text\": \"hi\"}",
        "事件與其參數：chat {\"text\": \"hi\"}",
    ),
    (
        "Answers with the saved examples of {}. Click to copy the URL.",
        "以 {} 已儲存的範例回應。點一下複製 URL。",
    ),
    ("Any server certificate is accepted", "接受任何伺服器憑證"),
    (
        "Any server certificate is accepted. Use only to diagnose CA problems.",
        "接受任何伺服器憑證。只在排查 CA 問題時使用。",
    ),
    ("API key", "API 金鑰"),
    (
        "API key (or use the Auth tab)",
        "API 金鑰（或使用「驗證」分頁）",
    ),
    ("apitool {} is the latest", "apitool {} 已是最新版本"),
    ("App settings", "應用程式設定"),
    ("Apply", "套用"),
    ("As received", "依收到的原樣"),
    (
        "Ask the server for its methods (gRPC reflection)",
        "向伺服器查詢方法（gRPC reflection）",
    ),
    (
        "Asked by {{?name}}; kept until apitool closes, never saved.",
        "由 {{?name}} 詢問；保留到 apitool 關閉，不會儲存。",
    ),
    (
        "Asking the server for its methods…",
        "正在向伺服器查詢方法…",
    ),
    ("Asserts", "斷言"),
    ("Auth", "驗證"),
    ("Auth URL", "授權網址"),
    ("Authorization code", "授權碼"),
    ("Auto", "自動"),
    (
        "Auto uses HTTP/2 when an https server offers it",
        "自動：https 伺服器支援時使用 HTTP/2",
    ),
    (
        "Available in every environment; an environment variable with the same name wins.",
        "所有環境都可使用；與環境變數同名時，以環境變數為準。",
    ),
    ("AWS Signature", "AWS 簽章"),
    (
        "Bad Gateway: a proxy or gateway got a bad answer from upstream.",
        "Bad Gateway：代理或閘道從上游收到無效的回應。",
    ),
    ("bad pattern", "樣式錯誤"),
    (
        "Bad Request: the server can't process the request as sent (syntax, framing, values).",
        "Bad Request：伺服器無法處理送出的請求（語法、格式或值有誤）。",
    ),
    ("Basic auth", "Basic 驗證"),
    ("Bearer token", "Bearer 權杖"),
    ("Beautify", "美化"),
    (
        "Below environments: an environment variable with the same name wins. Keep secrets in a secret environment.",
        "優先順序低於環境：同名的環境變數優先。秘密請放在秘密環境中。",
    ),
    // A file, or bytes typed as Base64 or hex.
    ("Binary", "二進位"),
    ("File", "檔案"),
    (
        "Base64 and hex are decoded and sent as a binary message",
        "Base64 與 hex 會解碼後以二進位訊息送出",
    ),
    (
        "Decoded when sent, once its variables are filled in.",
        "送出時代入變數後再解碼。",
    ),
    (
        "{} as sent · application/octet-stream unless set under Headers",
        "送出 {} · 未在 Headers 設定時為 application/octet-stream",
    ),
    ("Blue", "藍"),
    ("Body", "內容"),
    ("Body matches a JSON schema", "內容符合 JSON schema"),
    ("Body was too large to keep", "內容太大，未保存"),
    ("Bulk Edit", "批次編輯"),
    (
        "Bypass: localhost,127.0.0.1,.corp.local",
        "略過：localhost,127.0.0.1,.corp.local",
    ),
    ("Cancel", "取消"),
    ("cancelled", "已取消"),
    ("Certificate; empty sends none", "憑證；留空則不送"),
    ("Certificates", "憑證"),
    ("Check every day", "每天檢查"),
    ("Check every week", "每週檢查"),
    ("Check for updates", "檢查更新"),
    ("Check now", "立即檢查"),
    ("Check when apitool starts", "每次啟動 apitool 時檢查"),
    (
        "Checked in order before the one above",
        "依序比對，優先於上方的憑證",
    ),
    ("Choose a file", "選擇檔案"),
    ("Choose file…", "選擇檔案…"),
    ("Choose folder…", "選擇資料夾…"),
    ("Choose where to save", "選擇儲存位置"),
    ("Clean session", "清除工作階段"),
    ("Clear", "清除"),
    ("Clear all", "全部清除"),
    ("Clear history", "清除歷史紀錄"),
    (
        "Cleared this request's response history",
        "已清除此請求的回應紀錄",
    ),
    ("Click again to delete", "再點一次即刪除"),
    (
        "Click to add this field to the query.",
        "點一下把這個欄位加進查詢。",
    ),
    ("Click to load what was sent.", "點一下載入當時送出的內容。"),
    (
        "Click to replace the query with this field.",
        "點一下用這個欄位取代查詢。",
    ),
    ("Click to see it whole", "點選以檢視完整內容"),
    (
        "Client certificate (.pem with key, or .pfx / .p12)",
        "用戶端憑證（含金鑰的 .pem，或 .pfx / .p12）",
    ),
    ("Client", "用戶端"),
    ("Client credentials", "用戶端憑證"),
    (
        "Client error: the request is wrong or can't be fulfilled.",
        "用戶端錯誤：請求有誤或無法完成。",
    ),
    ("Client ID", "用戶端 ID"),
    ("client IP behind a proxy", "代理後方的用戶端 IP"),
    ("client name and version", "用戶端名稱與版本"),
    ("Client secret", "用戶端密鑰"),
    ("Close", "關閉"),
    ("Close (Esc)", "關閉 (Esc)"),
    ("Close ({})", "關閉 ({})"),
    ("Close all tabs", "關閉所有分頁"),
    ("Close other tabs", "關閉其他分頁"),
    ("Close tab", "關閉分頁"),
    ("Close tabs to the right", "關閉右側分頁"),
    ("Code", "程式碼"),
    ("Code font", "程式碼字型"),
    ("Collection settings", "集合設定"),
    ("Collection settings…", "集合設定…"),
    ("Collection, spec or HAR", "集合、規格或 HAR"),
    ("Collection: {}", "集合：{}"),
    ("Collections", "集合"),
    (
        "Colour this environment, e.g. prod in red",
        "為此環境上色，例如 prod 用紅色",
    ),
    (
        "Conflict: the request clashes with the resource's current state.",
        "Conflict：請求與資源目前的狀態衝突。",
    ),
    ("Connect", "連線"),
    (
        "Connect directly, ignoring OS settings.",
        "直接連線，忽略作業系統設定。",
    ),
    ("Connected {} s", "已連線 {} 秒"),
    (
        "Connected: a change is (un)subscribed as soon as you finish it.",
        "已連線：改完就會立即訂閱或取消訂閱。",
    ),
    ("Console ({})", "主控台 ({})"),
    ("Consumer key", "Consumer 金鑰"),
    ("Consumer secret", "Consumer 密鑰"),
    (
        "Content Too Large: the body is bigger than the server accepts.",
        "Content Too Large：內容超過伺服器接受的大小。",
    ),
    ("Cookie is set", "已設定 cookie"),
    ("Cookie jar", "Cookie 罐"),
    ("Cookies", "Cookie"),
    ("Cookies ({})", "Cookie ({})"),
    (
        "Cookies the server set, sent back automatically",
        "伺服器設定的 cookie，之後會自動送回",
    ),
    ("Copied as {}{}", "已複製為 {}{}"),
    ("Copied the mock server URL", "已複製模擬伺服器 URL"),
    (
        "Copied {} as a Postman environment",
        "已將 {} 複製為 Postman 環境",
    ),
    ("Copied {} snippet", "已複製 {} 程式碼片段"),
    ("Copy", "複製"),
    ("Copy as", "複製為"),
    ("Export", "匯出"),
    ("Postman collection", "Postman 集合"),
    ("Copy as Postman environment", "複製為 Postman 環境"),
    ("Copy body", "複製內容"),
    ("Copy the timeline", "複製時間軸"),
    (
        "Created: the request succeeded and a new resource was created.",
        "Created：請求成功，並建立了新資源。",
    ),
    (
        "credentials (or use the Auth tab)",
        "憑證（或使用「驗證」分頁）",
    ),
    ("CSV or JSON", "CSV 或 JSON"),
    ("curl import: {}", "curl 匯入：{}"),
    ("Dark", "深色"),
    ("Data file", "資料檔"),
    ("Default", "預設"),
    ("Default ({} s)", "預設 ({} 秒)"),
    ("Define in {}…", "在 {} 定義…"),
    ("Delay", "延遲"),
    ("Delete", "刪除"),
    ("Delete environment", "刪除環境"),
    ("Delete example", "刪除範例"),
    (
        "Delete the folder \"{}\" and everything in it?",
        "要刪除資料夾「{}」及其中所有內容嗎？",
    ),
    ("Delete the request \"{}\"?", "要刪除請求「{}」嗎？"),
    (
        "Delete these {} items, folders with everything in them?",
        "要刪除這 {} 個項目嗎？資料夾會連同其中所有內容一起刪除。",
    ),
    ("Delete these {} requests?", "要刪除這 {} 個請求嗎？"),
    ("Delete this response", "刪除此回應"),
    ("Delete {} items", "刪除 {} 個項目"),
    ("Description", "說明"),
    ("Digest auth", "Digest 驗證"),
    ("direct", "直接連線"),
    ("Discard", "捨棄"),
    ("Discard changes", "捨棄變更"),
    ("Disconnect", "中斷連線"),
    ("Docs", "文件"),
    ("Domain", "網域"),
    (
        "None yet: type a message and press the bookmark.",
        "還沒有：輸入訊息後按書籤圖示。",
    ),
    ("Drag to change the order", "拖曳以調整順序"),
    ("Don't check", "不檢查"),
    (
        "Downloads it in the background; it starts next time",
        "在背景下載，下次啟動時生效",
    ),
    ("Duplicate", "複製一份"),
    ("Duplicate environment", "複製環境"),
    ("Duplicate request", "複製請求"),
    ("Duplicated as \"{}\"", "已複製為「{}」"),
    ("Duplicate…", "複製一份…"),
    ("Duration (s)", "時長 (秒)"),
    (
        "Each Send is signed over its method, URL, query and form body with a fresh nonce and timestamp. Get the access token from the provider's sign-in flow first and paste it here.",
        "每次傳送都會以新的 nonce 與時間戳記，對方法、網址、查詢參數與表單內容簽章。請先從提供者的登入流程取得存取權杖，再貼到這裡。",
    ),
    (
        "Each Send is signed over its URL, headers and body. A signature lasts 15 minutes, so a copied snippet stops working after that. A body streamed from a file is sent as UNSIGNED-PAYLOAD, which S3 accepts.",
        "每次傳送都會對網址、標頭與內容簽章。簽章有效 15 分鐘，複製出去的程式碼過後就會失效。從檔案串流的內容會以 UNSIGNED-PAYLOAD 送出，S3 接受這種方式。",
    ),
    (
        "Earlier responses to this request (the last {})",
        "這個請求先前的回應（最近 {} 筆）",
    ),
    ("Edit", "編輯"),
    ("Edit Globals", "編輯全域變數"),
    ("Edit environment", "編輯環境"),
    (
        "Model Context Protocol: lets an LLM client such as Claude or Cursor use apitool",
        "Model Context Protocol：讓 Claude、Cursor 等 LLM 用戶端使用 apitool",
    ),
    ("Let it operate this window", "讓它操作這個視窗"),
    (
        "While apitool runs, the client can also see what's open, open requests, press Send and switch environments here. Without it, the client works on the workspace, even with apitool closed.",
        "apitool 執行時，用戶端還能看到這裡開著什麼、開啟請求、按下送出、切換環境。不勾選時，用戶端只操作工作區，apitool 關著也行。",
    ),
    ("Host", "主機"),
    (
        "127.0.0.1: this machine only. 0.0.0.0: other machines too; every call still needs the token.",
        "127.0.0.1：只限這台電腦。0.0.0.0：其他電腦也連得到；每次呼叫仍須帶 token。",
    ),
    ("Port", "連接埠"),
    ("auto", "自動"),
    (
        "auto: a free one the system picks, new each start",
        "自動：由系統挑一個空的，每次啟動都不同",
    ),
    ("Can't serve MCP: {}", "無法提供 MCP：{}"),
    ("apitool-cli isn't at {}", "{} 找不到 apitool-cli"),
    ("Serving this window on {}", "這個視窗在 {} 提供服務"),
    (
        "Run it in a terminal; add --scope user for every project.",
        "在終端機執行；加上 --scope user 讓每個專案都能用。",
    ),
    (
        "Add it to claude_desktop_config.json (Claude's Settings > Developer > Edit Config), then restart Claude.",
        "加進 claude_desktop_config.json（Claude 的 設定 > 開發者 > 編輯設定），再重新啟動 Claude。",
    ),
    (
        "Add it to ~/.cursor/mcp.json, or .cursor/mcp.json in a project.",
        "加進 ~/.cursor/mcp.json，或專案裡的 .cursor/mcp.json。",
    ),
    ("Copied the setup for {}", "已複製 {} 的設定"),
    ("Copied the token", "已複製權杖"),
    (
        "Sent as Authorization: Bearer by a client that connects over HTTP. New each start.",
        "以 HTTP 直接連線的 client 要放在 Authorization: Bearer 送出。每次啟動都會換新。",
    ),
    ("Sync", "同步"),
    ("owner/repository", "擁有者/儲存庫"),
    ("Branch", "分支"),
    ("Token kept; type to replace", "已保存 token；輸入可取代"),
    ("Personal access token", "個人存取權杖（PAT）"),
    (
        "A fine-grained token with Contents: read and write on the repository",
        "對該儲存庫有 Contents：讀寫權限的 fine-grained token",
    ),
    ("A token with the api scope", "具 api 範圍的 token"),
    ("Create a token", "建立 token"),
    (
        "Sign in to {} and make one with what sync needs filled in",
        "登入 {}，建立一個已填好同步所需權限的 token",
    ),
    ("Sync automatically", "自動同步"),
    ("Sync now", "立即同步"),
    (
        "To a private repository. What isn't ticked stays on this machine.",
        "同步到私有儲存庫。沒勾選的只留在這台電腦。",
    ),
    ("Also sync", "一併同步"),
    ("Secret values", "機密值"),
    ("OAuth tokens", "OAuth token"),
    ("Passphrase kept; type to replace", "已保存密語；輸入可取代"),
    (
        "Passphrase, the same on every machine",
        "密語（每台電腦相同）",
    ),
    (
        "What's ticked is encrypted with it before it leaves; another machine opens it with the same one.",
        "勾選的項目離開前會用它加密；其他電腦要用同一個密語才能打開。",
    ),
    ("Secret values of {}", "{} 的機密值"),
    (
        "Give the passphrase in Settings > Sync first",
        "請先在 設定 > 同步 填入密語",
    ),
    (
        "The passphrase doesn't open {} in the repository",
        "這個密語打不開儲存庫裡的 {}",
    ),
    (
        "Sync: name the repository and give a token in Settings first",
        "同步：請先在設定填好儲存庫與 token",
    ),
    ("Sync: {}", "同步：{}"),
    ("In step with {}", "已與 {} 一致"),
    (
        "Synced with {}: {} files came in, {} went out",
        "已與 {} 同步：拉下 {} 個檔案，推上 {} 個",
    ),
    ("Changed in both places", "兩邊都改了"),
    (
        "Since the last sync, these changed both in {} and here. Which should stay? The other side's changes to them are lost.",
        "上次同步後，這些在 {} 和這台電腦都被改過。要留哪一邊？另一邊對它們的修改會遺失。",
    ),
    ("Use the repository's", "用儲存庫的"),
    ("Keep this machine's", "保留這台的"),
    ("Sync with {} on {}", "與 {} 上的 {} 同步"),
    (
        "The workspace folder's files and apitool both changed: settle that first",
        "工作區資料夾的檔案與 apitool 都有變更：請先處理那邊",
    ),
    (
        "{} refused the token: check it, and that it may read and write the repository",
        "{} 拒絕了 token：請檢查它，以及它能否讀寫該儲存庫",
    ),
    ("{} has no branch {}", "{} 沒有分支 {}"),
    (
        "No repository {} on {}, or the token can't see it",
        "儲存庫 {} 不存在於 {}，或 token 看不到它",
    ),
    (
        "The repository changed while this synced: sync again",
        "同步途中儲存庫有了新變更：請再同步一次",
    ),
    ("Edit globals", "編輯全域變數"),
    ("Edit this environment's variables", "編輯此環境的變數"),
    ("Edit {}", "編輯 {}"),
    ("empty for two-legged", "two-legged 時留空"),
    ("End stream", "結束串流"),
    ("Enter to apply", "按 Enter 套用"),
    ("Environment", "環境"),
    ("Environment \"{}\" already exists", "環境「{}」已存在"),
    ("Environment: {}", "環境：{}"),
    ("Environment colour", "環境顏色"),
    ("Error", "錯誤"),
    ("Errors", "錯誤"),
    ("ETag from an earlier response", "先前回應的 ETag"),
    ("ETag to match", "要比對的 ETag"),
    ("Every {} minutes", "每 {} 分鐘"),
    ("Every hour", "每小時"),
    (
        "Each row sets a variable from the response, before the scripts and Asserts run, so they can use it. Key: the variable, e.g. token. Value: what to take, as in Asserts: res.body.access_token, res.body.items[0].id, res.headers['x-request-id'], res.status. It goes to the selected environment (the globals when there is none), as pm.environment.set does. A row that fails or finds nothing shows up under Tests.",
        "每一列從回應取一個值設成變數，在指令碼與斷言執行前完成，所以它們都能使用。鍵：變數名稱，例如 token。值：要取什麼，寫法同斷言：res.body.access_token、res.body.items[0].id、res.headers['x-request-id']、res.status。存進選定的環境（沒選環境時存進全域變數），與 pm.environment.set 相同。失敗或取不到值的列會出現在「測試」下。",
    ),
    ("Examples", "範例"),
    ("Filter messages", "篩選訊息"),
    (
        "Export the results as JUnit XML…",
        "將結果匯出為 JUnit XML…",
    ),
    ("Exported {} results to {}", "已將 {} 筆結果匯出到 {}"),
    (
        "Exported {} of {} results (the failures and the latest passes) to {}",
        "已將 {} 筆結果（共 {} 筆，含所有失敗與最近的通過）匯出到 {}",
    ),
    ("Expires", "到期"),
    (
        "Extra CA bundle (PEM file path)",
        "額外的 CA 憑證（PEM 檔案路徑）",
    ),
    ("Failed ({})", "失敗 ({})"),
    ("Failures only", "只看失敗"),
    ("Fetch a token first", "先取得權杖"),
    ("Fetch schema", "取得 schema"),
    (
        "Fetch the schema and click a field →\nor type a query here: its fields complete as you type.",
        "取得 schema 後點選欄位 →\n或在這裡輸入查詢：輸入時會補全欄位。",
    ),
    (
        "Fetch the schema to browse its queries, mutations and subscriptions.",
        "取得 schema 後即可瀏覽它的查詢、變更與訂閱。",
    ),
    ("File not found: {}", "找不到檔案：{}"),
    ("File path", "檔案路徑"),
    ("Fill in values", "填入值"),
    (
        "On: a variable or field added gets a random value of its type, as gRPC's Fill body does. Off: it starts empty.",
        "開啟：新加入的變數或欄位會填入該型別的隨機值，如同 gRPC 的 Fill body。關閉：從空值開始。",
    ),
    ("Leave it out", "不送出"),
    ("Sent; untick to leave it out", "會送出；取消勾選即不送出"),
    ("Not sent; tick to add it", "不會送出；勾選即加入"),
    ("not in its type", "不在此型別中"),
    ("Fill in its fields", "填入欄位"),
    ("Fill body", "填入內容"),
    ("Filter by name", "依名稱篩選"),
    ("Filter fields", "篩選欄位"),
    ("Filter: $.items[*].id", "篩選：$.items[*].id"),
    ("Filter: //item/@id", "篩選：//item/@id"),
    ("Filters used before", "先前用過的篩選"),
    ("Find", "尋找"),
    ("Find ({})", "尋找 ({})"),
    ("Find in the response", "在回應中尋找"),
    ("Fold", "收合"),
    ("Folder", "資料夾"),
    ("Folder scripts run first: {}", "會先執行資料夾指令碼：{}"),
    ("Folder settings", "資料夾設定"),
    ("Folder settings…", "資料夾設定…"),
    ("Folder: {}", "資料夾：{}"),
    ("Follow redirects", "跟隨重新導向"),
    ("for CORS", "用於 CORS"),
    (
        "Forbidden: the credentials are known but not allowed to do this.",
        "Forbidden：憑證有效，但沒有權限執行此操作。",
    ),
    ("Form", "表單"),
    (
        "Found: the resource is at another URL for now.",
        "Found：資源暫時位於另一個 URL。",
    ),
    ("from {}", "來自 {}"),
    (
        "Gateway Timeout: a proxy or gateway got no answer from upstream in time.",
        "Gateway Timeout：代理或閘道沒有及時收到上游的回應。",
    ),
    ("General", "一般"),
    ("Globals", "全域變數"),
    ("Go to tab 1–8, the last", "跳到第 1–8 個分頁、最後一個"),
    (
        "Go to a request, folder or action",
        "前往請求、資料夾或動作",
    ),
    (
        "Go to a request, folder, environment or action",
        "前往請求、資料夾、環境或動作",
    ),
    (
        "Gone: the resource was here and was removed for good.",
        "Gone：資源曾經存在，但已永久移除。",
    ),
    ("Grant", "授權類型"),
    ("Green", "綠"),
    ("Header", "標頭"),
    ("Header is present", "有某個標頭"),
    ("Headers", "標頭"),
    ("Headers ({})", "標頭 ({})"),
    ("Headers {}\nBody {}", "標頭 {}\n內容 {}"),
    ("History", "歷史紀錄"),
    ("History: {}", "歷史紀錄：{}"),
    ("HTTP date", "HTTP 日期"),
    ("HTTP version", "HTTP 版本"),
    (
        "http://127.0.0.1:<any free port>/callback",
        "http://127.0.0.1:<任一可用連接埠>/callback",
    ),
    (
        "http://user:pass@proxy.corp:8080 or socks5h://host:1080",
        "http://user:pass@proxy.corp:8080 或 socks5h://host:1080",
    ),
    (
        "http://wpad/proxy.pac  or  C:\\path\\proxy.pac",
        "http://wpad/proxy.pac  或  C:\\path\\proxy.pac",
    ),
    ("id for this request", "此請求的 ID"),
    (
        "id to trace a call across services",
        "跨服務追蹤呼叫用的 ID",
    ),
    ("Implicit", "隱含"),
    ("Import", "匯入"),
    (
        "Import (Postman, OpenAPI, Swagger)",
        "匯入（Postman、OpenAPI、Swagger）",
    ),
    ("Import a collection or spec", "匯入集合或規格"),
    ("Imported curl command", "已匯入 curl 指令"),
    ("Imported environment \"{}\"", "已匯入環境「{}」"),
    ("Imported {} environments", "已匯入 {} 個環境"),
    ("Imported {} globals", "已匯入 {} 個全域變數"),
    (
        "Imported {} requests into \"{}\"",
        "已匯入 {} 個請求到「{}」",
    ),
    ("Import…", "匯入…"),
    (
        "Informational: the request was received and goes on.",
        "資訊：請求已收到，處理中。",
    ),
    ("Inherit from parent", "繼承上層"),
    ("Install", "安裝"),
    ("Install automatically", "自動安裝"),
    ("Installing {}…", "正在安裝 {}…"),
    ("Interface font", "介面字型"),
    (
        "Internal Server Error: the server failed while handling the request.",
        "Internal Server Error：伺服器處理請求時發生錯誤。",
    ),
    (
        "Introspect using this request's URL, headers and auth",
        "以此請求的網址、標頭與驗證查詢 schema",
    ),
    ("it has no pages", "沒有任何頁面"),
    ("it has no size", "沒有尺寸"),
    ("it's encrypted", "已加密"),
    ("Iterations", "迭代次數"),
    ("JSON body has a property", "JSON 內容有某個屬性"),
    ("JSON, YAML or a path", "JSON、YAML 或路徑"),
    ("JUnit XML", "JUnit XML"),
    ("just now", "剛剛"),
    ("JWT Bearer", "JWT Bearer"),
    ("Keep alive", "保持連線"),
    ("Keep this workspace", "保留此工作區"),
    ("Keeping the response: {}", "保存回應時發生錯誤：{}"),
    (
        "Kept on this machine only (in apitool.db), never written to the files. Overrides shared values; values set by scripts land here.",
        "只存在這台電腦（apitool.db），不會寫入檔案。會覆蓋共用的值；指令碼設定的值也存在這裡。",
    ),
    ("Key", "鍵"),
    ("Key-Value Edit", "鍵值編輯"),
    (
        "key: value, one per line; // in front turns a line off",
        "key: value，一行一個；行首加 // 可停用該行",
    ),
    (
        "Key: what to check, e.g. res.status, res.body.items.length, res.body[0].id, res.headers['content-type'], res.responseTime.\nValue: an operator and what to compare with, e.g. eq 200, neq, gt 0, gte, lt 500, lte, in 200,201, notIn, contains ok, notContains, length 3, matches ^ok, notMatches, startsWith, endsWith, between 1,10, isEmpty, isNotEmpty, isNull, isUndefined, isDefined, isTruthy, isFalsy, isJson, isNumber, isString, isBoolean, isArray; no operator is eq. {{variables}} work. Each row shows up under Tests.",
        "鍵：要檢查的項目，例如 res.status, res.body.items.length, res.body[0].id, res.headers['content-type'], res.responseTime。\n值：運算子與比較的值，例如 eq 200, neq, gt 0, gte, lt 500, lte, in 200,201, notIn, contains ok, notContains, length 3, matches ^ok, notMatches, startsWith, endsWith, between 1,10, isEmpty, isNotEmpty, isNull, isUndefined, isDefined, isTruthy, isFalsy, isJson, isNumber, isString, isBoolean, isArray；沒寫運算子就是 eq。可使用 {{variables}}。每一列都會出現在「測試」下。",
    ),
    ("Language", "語言"),
    ("Last response", "上次回應"),
    ("Last will", "遺囑訊息"),
    ("Light", "淺色"),
    ("Load test", "負載測試"),
    (
        "Load test: many virtual users sending this request",
        "負載測試：多個虛擬使用者同時送出此請求",
    ),
    ("Log a variable", "記錄變數"),
    ("Manual", "手動"),
    ("Manual proxy", "手動代理伺服器"),
    (
        "Markdown. A folder's right-click menu exports these docs as Markdown or OpenAPI.",
        "Markdown 格式。資料夾的右鍵選單可將這些文件匯出為 Markdown 或 OpenAPI。",
    ),
    ("Match case", "大小寫須相符"),
    ("Max", "最大"),
    ("Maximum redirects", "最多重新導向次數"),
    ("Mean", "平均"),
    ("media type of the body", "內容的媒體類型"),
    ("media types the client takes", "用戶端接受的媒體類型"),
    ("Message", "訊息"),
    ("Message (JSON)", "訊息（JSON）"),
    (
        "Method Not Allowed: the URL exists but not for this method.",
        "Method Not Allowed：URL 存在，但不支援此方法。",
    ),
    ("Mock server for {} at {}", "{} 的模擬伺服器：{}"),
    ("Mock server stopped", "模擬伺服器已停止"),
    ("Mock server: {}", "模擬伺服器：{}"),
    ("Mock {}", "模擬 {}"),
    ("Mock: {}", "模擬：{}"),
    ("More", "更多"),
    ("More save options", "更多儲存選項"),
    ("More send options", "更多傳送選項"),
    (
        "Moved Permanently: the resource has a new URL for good.",
        "Moved Permanently：資源已永久移到新的 URL。",
    ),
    ("Moved to \"{}\"", "已移到「{}」"),
    ("Moved {} items to \"{}\"", "已將 {} 個項目移到「{}」"),
    ("Multipart", "Multipart"),
    ("Mutation", "變更"),
    ("Name", "名稱"),
    ("name can't be empty", "名稱不能空白"),
    (
        "name can't be empty or start/end with '.'",
        "名稱不能空白，也不能以「.」開頭或結尾",
    ),
    ("name can't contain '{}'", "名稱不能包含「{}」"),
    ("Network", "網路"),
    ("Network settings", "網路設定"),
    (
        "Network settings (proxy, certificates)",
        "網路設定（代理伺服器、憑證）",
    ),
    ("Network settings applied", "已套用網路設定"),
    ("Network settings: {}", "網路設定：{}"),
    ("New collection", "新增集合"),
    (
        "New collection: requests with their own variables, auth and scripts",
        "新增集合：一組請求，有自己的變數、驗證與指令碼",
    ),
    ("New environment", "新增環境"),
    ("New folder", "新增資料夾"),
    ("New request", "新增請求"),
    ("New request ({})", "新增請求 ({})"),
    ("Next", "下一個"),
    ("Next match ({})", "下一個符合項 ({})"),
    ("Next page", "下一頁"),
    ("Next tab", "下一個分頁"),
    ("No auth", "不驗證"),
    (
        "No Content: succeeded, with no body to return.",
        "No Content：成功，沒有內容可回傳。",
    ),
    (
        "No cookies yet. Set-Cookie responses fill the jar.",
        "還沒有 Cookie。回應裡的 Set-Cookie 會放進罐子。",
    ),
    ("No environment", "無環境"),
    (
        "No environment selected: pm.environment.set was stored as a global",
        "未選擇環境：pm.environment.set 已存為全域變數",
    ),
    (
        "No folder above sets auth, so none is sent.",
        "上層資料夾都沒有設定驗證，因此不送驗證。",
    ),
    ("No proxy", "不用代理伺服器"),
    ("no request is open", "沒有開啟的請求"),
    ("No requests to run here.", "這裡沒有可執行的請求。"),
    ("no requests under {}", "{} 底下沒有請求"),
    ("No requests yet.", "還沒有請求。"),
    (
        "No variables yet. Add some to an environment or to Globals.",
        "還沒有變數。可以加到環境或全域變數裡。",
    ),
    ("None", "無"),
    ("not a PDF it can read", "不是可讀取的 PDF"),
    ("not in the query", "查詢裡沒有"),
    (
        "Not Acceptable: nothing matches the Accept headers.",
        "Not Acceptable：沒有符合 Accept 標頭的內容。",
    ),
    ("Not connected", "未連線"),
    (
        "Not Found: nothing at this URL.",
        "Not Found：這個 URL 沒有東西。",
    ),
    (
        "Not Implemented: the server doesn't support this method.",
        "Not Implemented：伺服器不支援此方法。",
    ),
    (
        "Not Modified: the cached copy is still good.",
        "Not Modified：快取的副本仍然有效。",
    ),
    ("Not a number", "不是數字"),
    ("Not a whole number", "不是整數"),
    ("Not valid JSON", "不是有效的 JSON"),
    ("Not valid JSON: {}", "不是有效的 JSON：{}"),
    (
        "Note: the runner uses saved files; unsaved edits are not included.",
        "注意：執行器使用已儲存的檔案，不含未儲存的變更。",
    ),
    ("Nothing matches.", "沒有符合的項目。"),
    (
        "Nothing to mock yet: send a request, then \"Save as example\"",
        "還沒有可模擬的內容：先送出請求，再按「存為範例」",
    ),
    ("OAuth 1.0", "OAuth 1.0"),
    ("OAuth 2.0", "OAuth 2.0"),
    (
        "Off accepts any certificate, for this request only",
        "關閉時接受任何憑證，只限這個請求",
    ),
    (
        "Off shows the 3xx response itself",
        "關閉時顯示 3xx 回應本身",
    ),
    (
        "Off: the broker keeps subscriptions and queued messages for this client ID",
        "關閉：broker 會為這個用戶端 ID 保留訂閱與排隊中的訊息",
    ),
    ("OK: the request succeeded.", "OK：請求成功。"),
    ("Off", "關閉"),
    (
        "On: a clicked field joins the query, so one query can ask for several. Off: a root field replaces the query.",
        "開啟：點的欄位會加進查詢，一個查詢可同時查多個。關閉：點根欄位會取代整個查詢。",
    ),
    ("Open in browser", "在瀏覽器開啟"),
    ("Open in new tab", "在新分頁開啟"),
    ("optional", "選填"),
    (
        "Optional CSV (with header row) or JSON array; one iteration per row",
        "選填：CSV（含標題列）或 JSON 陣列；每一列執行一次",
    ),
    ("optional, space separated", "選填，以空格分隔"),
    ("Orange", "橘"),
    ("PAC script", "PAC 指令碼"),
    ("Page {} of {}", "第 {} 頁，共 {} 頁"),
    ("Params", "參數"),
    (
        "Partial Content: only the requested range is returned.",
        "Partial Content：只回傳請求的範圍。",
    ),
    ("Passed ({})", "通過 ({})"),
    ("Password", "密碼"),
    ("Path", "路徑"),
    ("Path Variables", "路徑變數"),
    (
        "path/to/file (relative to the workspace)",
        "path/to/file（相對於工作區）",
    ),
    ("Payload", "內容"),
    (
        "Permanent Redirect: repeat the same request at another URL, from now on.",
        "Permanent Redirect：從此改向另一個 URL 重送同一個請求。",
    ),
    ("PFX password", "PFX 密碼"),
    (
        "Pick the exported file instead of pasting it",
        "選擇匯出的檔案，不必貼上",
    ),
    ("Post-response", "回應後"),
    ("Pre-request", "請求前"),
    (
        "Press Connect to open the stream. Scripts don't run for WebSocket/SSE.",
        "按「連線」開啟串流。WebSocket/SSE 不執行指令碼。",
    ),
    (
        "Press Connect to reach the broker; it subscribes to the topics in the Topics tab. Scripts don't run for MQTT.",
        "按「連線」連到 broker，並訂閱「主題」分頁裡的主題。MQTT 不執行指令碼。",
    ),
    (
        "Press Connect to start the call. The body is the first message (for a client stream, an array is several, empty is none). Scripts don't run for streams.",
        "按「連線」開始呼叫。內容是第一則訊息（用戶端串流時，陣列代表多則，空白代表沒有）。串流不執行指令碼。",
    ),
    (
        "Press Send or {} to see the response.",
        "按「傳送」或 {} 查看回應。",
    ),
    ("Pretty", "美化"),
    ("Prev", "上一個"),
    ("Preview", "預覽"),
    ("Previous match ({})", "上一個符合項 ({})"),
    ("Previous page", "上一頁"),
    ("Previous tab", "上一個分頁"),
    ("Private key", "私密金鑰"),
    ("Properties", "屬性"),
    ("Proto", "Proto"),
    ("Proxy", "代理伺服器"),
    ("Close drops what isn't applied", "未套用的變更在關閉時捨棄"),
    (
        "Network settings not applied: closed without Apply",
        "網路設定未套用：關閉前沒有按套用",
    ),
    ("Publish needs a topic", "發布需要指定主題"),
    ("Publish to", "發布到"),
    ("Purple", "紫"),
    ("Put a cookie in the jar", "在 cookie jar 放入 cookie"),
    ("Query", "查詢"),
    ("Query params", "查詢參數"),
    ("Quick look", "快速檢視"),
    (
        "Quick look: every variable in scope",
        "快速檢視：目前範圍內的所有變數",
    ),
    ("random", "隨機"),
    ("Raw", "原始"),
    ("raw bytes", "原始位元組"),
    ("Read the changed workspace files", "已讀入變更的工作區檔案"),
    ("Reading that response: {}", "讀取該回應時發生錯誤：{}"),
    ("Realm", "Realm"),
    ("Recent", "最近"),
    ("Red", "紅"),
    ("Redirect URI", "重新導向 URI"),
    (
        "Redirection: more action is needed to complete the request.",
        "重新導向：需要進一步的動作才能完成請求。",
    ),
    ("Refresh", "重新整理"),
    ("Region", "區域"),
    ("Regular expression", "規則運算式"),
    ("Release notes", "版本說明"),
    ("Reload", "重新載入"),
    ("Reload .proto", "重新載入 .proto"),
    (
        "Reloaded {}: changed on disk",
        "已重新載入 {}：磁碟上的檔案已變更",
    ),
    (
        "Required: the server refuses the query without it.",
        "必填：少了它伺服器會拒絕這個查詢。",
    ),
    ("Remove", "移除"),
    ("Rename", "重新命名"),
    ("Reopen closed tab", "重新開啟關閉的分頁"),
    ("Repeat every", "每隔一段時間重送"),
    (
        "Replace the body with a request message, each field a random value of its type",
        "以請求訊息取代內容，每個欄位依其型別填入隨機值",
    ),
    ("Request cancelled", "已取消請求"),
    ("Request failed", "請求失敗"),
    (
        "Request Timeout: the server gave up waiting for the request.",
        "Request Timeout：伺服器等待請求逾時。",
    ),
    ("Requests", "請求數"),
    ("Requests ({}/{})", "請求 ({}/{})"),
    (
        "Requests set to \"Inherit from parent\" use this.",
        "設為「繼承上層」的請求會使用這個。",
    ),
    (
        "Requests you send show up here.",
        "送出的請求會出現在這裡。",
    ),
    ("Received only", "只看收到"),
    ("Reset", "重設"),
    (
        "Reset to the tree's order, all ticked",
        "恢復成樹狀清單的順序並全部勾選",
    ),
    ("Reset to defaults", "重設為預設值"),
    ("Response matches its schema", "回應符合 schema"),
    ("Response time is below 500 ms", "回應時間低於 500 ms"),
    ("Restart now", "立即重新啟動"),
    ("Retain", "保留"),
    ("Reused an open connection\n", "沿用已開啟的連線\n"),
    ("Run", "執行"),
    ("Run all collections", "執行所有集合"),
    (
        "Run cancelled; variable changes from it were discarded.",
        "已取消執行；這次執行的變數變更已捨棄。",
    ),
    ("Run collection", "執行集合"),
    ("Run folder", "執行資料夾"),
    ("Run: {}", "執行：{}"),
    ("Save", "儲存"),
    ("Save a JSON value to the environment", "將 JSON 值存入環境"),
    ("Save as", "另存為"),
    ("Save as example", "存為範例"),
    (
        "Save as example: keep this response with the request",
        "存為範例：將此回應與請求一起保存",
    ),
    ("Save as…", "另存為…"),
    ("Save changes", "儲存變更"),
    ("Save changes to \"{}\"?", "要儲存「{}」的變更嗎？"),
    (
        "Save or close the requests with unsaved edits first",
        "請先儲存或關閉有未儲存修改的請求",
    ),
    ("Save response body", "儲存回應內容"),
    (
        "Save the body to a file, as received",
        "將內容依收到的原樣存成檔案",
    ),
    (
        "Save the response straight to a file, for big ones",
        "將回應直接存成檔案，適合大型回應",
    ),
    (
        "Save these edits as a new request; this one stays as saved",
        "把這些修改存成新的請求；這個請求維持原樣",
    ),
    ("Save to file…", "儲存到檔案…"),
    (
        "Save, then copy into a new environment (e.g. dev → prod)",
        "儲存後複製成新環境（例如 dev → prod）",
    ),
    ("Saved as \"{}\"", "已另存為「{}」"),
    ("Saved example \"{}\"", "已儲存範例「{}」"),
    ("Saved {}", "已儲存 {}"),
    ("Saved {}{}", "已儲存 {}{}"),
    ("Nothing written yet.", "尚未撰寫內容。"),
    ("Write", "撰寫"),
    (
        "; left out {} SSE, gRPC, MQTT or Socket.IO requests, which Insomnia can't hold",
        "；略過 {} 個 Insomnia 無法容納的 SSE、gRPC、MQTT 或 Socket.IO 請求",
    ),
    (
        "; left out {} streams and gRPC requests, which it can't hold",
        "；略過 {} 個無法容納的串流與 gRPC 請求",
    ),
    (
        "; left out {} it can't describe: streams, gRPC, or a second request to the same method and path",
        "；略過 {} 個無法描述的請求：串流、gRPC，或同一方法與路徑的第二個請求",
    ),
    (
        "Saved {} (cut at 16 MiB) to {}",
        "已將 {}（截斷於 16 MiB）儲存到 {}",
    ),
    ("Saved {} to {}", "已將 {} 儲存到 {}"),
    ("Save…", "儲存…"),
    ("Saving environment: {}", "儲存環境時發生錯誤：{}"),
    ("Saving globals: {}", "儲存全域變數時發生錯誤：{}"),
    ("Schema", "Schema"),
    ("Scope", "範圍"),
    ("Scripts", "指令碼"),
    ("Secret", "機密"),
    ("Secret key", "秘密金鑰"),
    (
        "See Other: get the result from another URL with GET.",
        "See Other：請用 GET 從另一個 URL 取得結果。",
    ),
    (
        "Select a request on the left, or press {} for a new one.",
        "從左側選一個請求，或按 {} 新增。",
    ),
    ("Select method", "選擇方法"),
    ("Select the URL", "選取 URL"),
    ("Send", "傳送"),
    ("Send and download", "傳送並下載"),
    ("Send and download…", "傳送並下載…"),
    (
        "Send now and again at this interval, until stopped",
        "現在傳送，之後每隔這段時間再送，直到停止",
    ),
    (
        "Send opens your browser to sign in (with PKCE); the token is then reused until it expires or is rejected. Register the redirect URI with the provider; the client secret is only for confidential clients.",
        "傳送時會開啟瀏覽器登入（使用 PKCE），之後重複使用權杖，直到過期或被拒。請向提供者註冊重新導向 URI；用戶端密碼只用於機密用戶端。",
    ),
    (
        "Send opens your browser to sign in; the provider hands the token straight back to this machine. It is reused until it expires or is rejected, then you sign in again (the implicit grant has no refresh tokens).",
        "傳送時會開啟瀏覽器登入，提供者會把權杖直接交回本機。權杖會重複使用直到過期或被拒，之後需重新登入（implicit 授權沒有 refresh token）。",
    ),
    ("Send request", "傳送請求"),
    (
        "Send stored cookies and keep the ones the server sets",
        "送出已存的 Cookie，並保留伺服器設定的 Cookie",
    ),
    ("Sending every {} s", "每 {} 秒傳送一次"),
    ("Sending… {} s", "傳送中… {} 秒"),
    (
        "Sends the current draft (variables resolved once). Scripts are skipped.",
        "傳送目前的草稿（變數只解析一次），略過指令碼。",
    ),
    (
        "Sent back to matching URLs, unless a request sets its own Cookie header.",
        "會送回相符的網址，除非請求自己設了 Cookie 標頭。",
    ),
    (
        "Sent with every message you publish.",
        "每則發布的訊息都會帶上。",
    ),
    (
        "Server error: the server failed to fulfil a valid request.",
        "伺服器錯誤：伺服器無法完成有效的請求。",
    ),
    ("Service", "服務"),
    (
        "Service Unavailable: overloaded or down for maintenance; see Retry-After.",
        "Service Unavailable：伺服器過載或維護中；請看 Retry-After。",
    ),
    ("Session token", "工作階段權杖"),
    ("Set a request header", "設定請求標頭"),
    ("Set an environment variable", "設定環境變數"),
    (
        "set by the Body tab's form-data mode",
        "由「內容」分頁的 form-data 模式設定",
    ),
    ("Set by the number of data rows", "由資料列數決定"),
    ("Sets the Content-Type header", "會設定 Content-Type 標頭"),
    ("Settings", "設定"),
    (
        "Settings (theme, fonts, text size)",
        "設定（主題、字型、文字大小）",
    ),
    ("Settings ({})", "設定 ({})"),
    ("Shared", "共用"),
    (
        "Shared by every request in this folder and its subfolders. Written to its .folder.toml, for git.",
        "此資料夾與其子資料夾中的所有請求共用。寫入其 .folder.toml，供 git 使用。",
    ),
    ("Show anyway", "仍然顯示"),
    (
        "Show it in your browser, or the system's PDF viewer",
        "在瀏覽器或系統的 PDF 檢視器中開啟",
    ),
    (
        "Show or hide collections and history, for more room",
        "顯示或隱藏集合與歷史紀錄，騰出空間",
    ),
    ("Show or hide the sidebar", "顯示或隱藏側邊欄"),
    ("Hide the response", "隱藏回應"),
    ("Show the response", "顯示回應"),
    ("Response", "回應"),
    ("Sending…", "傳送中…"),
    ("Log", "記錄"),
    ("Warnings", "警告"),
    ("Info", "一般"),
    ("Debug", "除錯"),
    ("Trace", "追蹤"),
    ("Open the log's folder", "開啟記錄檔所在的資料夾"),
    (
        "How much goes into apitool.log. Debug adds each step of a request or stream: proxy picked, connection, TLS, redirects, status. Trace also each message's size. Headers, bodies and tokens are never written, nor a URL's query.",
        "apitool.log 要記錄多少。「除錯」會加上請求與串流的每個步驟：選用的代理伺服器、連線、TLS、重新導向、狀態碼；「追蹤」再加上每則訊息的大小。標頭、內文與權杖一律不寫入，URL 的查詢字串也不會。",
    ),
    ("Show the whole body", "顯示完整內容"),
    (
        "Showing {} of {}: failures and the latest passes.",
        "顯示 {} / {} 筆：失敗的與最新通過的。",
    ),
    ("Side by side", "左右並排"),
    ("Side by side or stacked", "左右並排或上下堆疊"),
    ("Sidebar", "側邊欄"),
    ("Signature", "簽章"),
    (
        "Signed on each Send and sent as Authorization: Bearer. Variables work in the payload ({{$timestamp}} for iat); keep the secret in a secret environment.",
        "每次傳送時簽章，並以 Authorization: Bearer 送出。payload 可使用變數（iat 可用 {{$timestamp}}）；密鑰請放在秘密環境中。",
    ),
    ("Skip TLS certificate verification", "略過 TLS 憑證驗證"),
    ("Snippets", "程式碼片段"),
    ("Start mock server", "啟動模擬伺服器"),
    ("Status", "狀態"),
    ("Status code is 200", "狀態碼為 200"),
    ("Status codes", "狀態碼"),
    ("Save this message in the request", "把這則訊息存進請求"),
    ("Saved messages ({})", "已存的訊息 ({})"),
    ("Sent only", "只看送出"),
    ("Stop", "停止"),
    ("Stop at the first failure", "遇到第一個失敗就停止"),
    ("stopped at the first failure", "已在第一個失敗處停止"),
    ("Stop repeat", "停止重送"),
    (
        "Stop sending; the server can still reply",
        "停止傳送；伺服器仍可回覆",
    ),
    (
        "Stored on this machine only (in apitool.db), never exported.",
        "只存在這台電腦（apitool.db），不會匯出。",
    ),
    (
        "Subscribed to on Connect. + matches one level, # everything below.",
        "連線時訂閱。+ 符合一層，# 符合以下所有層。",
    ),
    (
        "Success: the request was received, understood and accepted.",
        "成功：請求已收到、理解並接受。",
    ),
    ("System", "系統"),
    ("System proxy", "系統代理伺服器"),
    ("temporary credentials only", "僅限臨時憑證"),
    (
        "Temporary Redirect: repeat the same request at another URL.",
        "Temporary Redirect：向另一個 URL 重送同一個請求。",
    ),
    ("Tests ({}/{})", "測試 ({}/{})"),
    ("Tests {}/{}", "測試 {}/{}"),
    ("Tests: {}/{} passed", "測試：{}/{} 通過"),
    ("Text", "文字"),
    ("Text size", "文字大小"),
    (
        "The broker keeps it for whoever subscribes later",
        "broker 會保留給之後訂閱的人",
    ),
    (
        "The broker publishes it if the connection drops without a goodbye",
        "連線未正常結束時，broker 會發布這則訊息",
    ),
    (
        "The collection runner is already running.",
        "集合執行器已在執行中。",
    ),
    (
        "The file's bytes are the body, as they are. Content-Type comes from the extension unless set under Headers. Or drop a file here.",
        "檔案的位元組原封不動作為內容。除非在標頭裡設定，Content-Type 依副檔名決定。也可以把檔案拖到這裡。",
    ),
    (
        "The files in {} changed outside apitool (a git pull?), and so did this workspace since they were last in step. Which one should stay? The other one's changes are lost, unless git has them.",
        "{} 中的檔案在 apitool 之外被修改了（git pull？），此工作區自上次同步後也有變更。要保留哪一邊？另一邊的變更會遺失，除非 git 裡還留著。",
    ),
    (
        "The OS certificate store is always trusted; these are added on top.",
        "一律信任作業系統的憑證存放區；這些是額外加入的。",
    ),
    ("the page linking here", "連到這裡的頁面"),
    (
        "The query declares no variables. Declare them as in query Users($first: Int), or click a field with arguments in the schema.",
        "查詢沒有宣告變數。可以像 query Users($first: Int) 這樣宣告，或在結構裡點有參數的欄位。",
    ),
    ("The request isn't open any more", "這個請求已經沒有開啟"),
    (
        "These variables aren't a JSON object: fix them under JSON.",
        "這些變數不是 JSON 物件：請切到 JSON 修正。",
    ),
    ("The response beside the request", "回應放在請求右側"),
    ("The response under the request", "回應放在請求下方"),
    ("Response below", "回應在下方"),
    (
        "The response was over {} and wasn't kept while the tab was in the background: send again to see it",
        "回應超過 {}，分頁在背景時沒有保留：請重新傳送以查看",
    ),
    (
        "The server has {} services (gRPC reflection)",
        "伺服器有 {} 個服務（gRPC reflection）",
    ),
    (
        "The system's proxy script {} can't be used, so requests go out directly (as in a browser).\n{}",
        "系統的代理伺服器 PAC 指令碼 {} 無法使用，所以請求改為直接連線（與瀏覽器相同）。\n{}",
    ),
    (
        "The token is fetched on Send and reused until it expires or is rejected.",
        "傳送時取得權杖，並重複使用到過期或被拒絕為止。",
    ),
    (
        "The window is too narrow: the response is under the request until it is wider",
        "視窗太窄：回應會放在請求下方，直到視窗夠寬",
    ),
    ("Theme", "主題"),
    (
        "This body is {}: showing it takes a moment and a lot of memory.",
        "此內容有 {}，顯示需要一些時間與大量記憶體。",
    ),
    (
        "This request as curl, Python, Go, … to copy",
        "將此請求轉成 curl、Python、Go 等程式碼以便複製",
    ),
    (
        "The ticked requests run in the order on the left, with the selected environment.",
        "勾選的請求依左側順序、以選定的環境執行。",
    ),
    ("This request has no body.", "這個請求沒有內容。"),
    (
        "This snippet is {}: too big to show here. Copy still copies all of it.",
        "此程式碼有 {}，太大無法在此顯示。「複製」仍會複製全部。",
    ),
    (
        "This SVG can't be drawn ({}). Raw shows its text.",
        "無法繪製此 SVG（{}）。「原始」會顯示其文字。",
    ),
    ("Throughput", "吞吐量"),
    ("Tick the requests to run.", "請勾選要執行的請求。"),
    ("Timeline", "時間軸"),
    ("Timeout", "逾時"),
    (
        "Tip: use {{variables}} from a secret environment so credentials never reach git.",
        "提示：使用機密環境裡的 {{variables}}，讓帳密永遠不會進到 git。",
    ),
    ("TLS verify OFF", "未驗證 TLS"),
    ("Token", "權杖"),
    ("Token secret", "權杖密鑰"),
    ("Token URL", "權杖網址"),
    (
        "Too Many Requests: rate limited; see Retry-After.",
        "Too Many Requests：已被限流；請看 Retry-After。",
    ),
    ("Topics", "主題"),
    ("Types ({})", "型別 ({})"),
    (
        "Unauthorized: credentials are missing or wrong.",
        "Unauthorized：缺少憑證或憑證錯誤。",
    ),
    ("Undefined: {}", "未定義：{}"),
    ("Unfold", "展開"),
    (
        "Unprocessable Content: well-formed, but the values don't pass validation.",
        "Unprocessable Content：格式正確，但值沒有通過驗證。",
    ),
    ("Unsaved changes", "尚未儲存的變更"),
    (
        "Unsupported Media Type: the server doesn't take this Content-Type.",
        "Unsupported Media Type：伺服器不接受此 Content-Type。",
    ),
    ("Update", "更新"),
    ("Updates", "更新"),
    ("Use the files", "使用檔案"),
    ("Username", "使用者名稱"),
    (
        "Username and password go in Auth (Basic). ws:// and wss:// carry MQTT over WebSocket (path as the broker says, often /mqtt). mqtts:// and wss:// use the certificate settings in Network settings, and the connection goes through its proxy.",
        "使用者名稱與密碼請填在「驗證」（Basic）。ws:// 與 wss:// 透過 WebSocket 傳送 MQTT（路徑依 broker 而定，通常是 /mqtt）。mqtts:// 與 wss:// 使用網路設定中的憑證，連線也會經過其中的代理伺服器。",
    ),
    (
        "Uses the OS proxy settings and HTTP(S)_PROXY / NO_PROXY variables.",
        "使用作業系統的代理伺服器設定，以及 HTTP(S)_PROXY / NO_PROXY 變數。",
    ),
    (
        "Uses {} from folder \"{}\".",
        "使用 {}，設定於資料夾「{}」。",
    ),
    ("Value", "值"),
    ("Values for this request", "這個請求的值"),
    ("Variables", "變數"),
    ("Vars", "回應變數"),
    ("Variables ({})", "變數 ({})"),
    (
        "Variables are filled in; scripts don't run.",
        "已代入變數；不執行指令碼。",
    ),
    (
        "Variables available in every environment",
        "所有環境都能用的變數",
    ),
    ("Variables in scope", "可用的變數"),
    ("Variables not loaded: {}", "變數未載入：{}"),
    ("Verify TLS certificate", "驗證 TLS 憑證"),
    ("Version", "版本"),
    ("Virtual users", "虛擬使用者"),
    (
        "## What it's for\n\n## When to use it\n\n## What comes back\n",
        "## 用途\n\n## 使用時機\n\n## 會回傳什麼\n",
    ),
    ("Insert template", "插入範本"),
    (
        "What this response's Set-Cookie headers set",
        "此回應的 Set-Cookie 標頭設定了什麼",
    ),
    (
        "What was sent, redirects, and what came back",
        "送出的內容、重新導向與收到的回應",
    ),
    (
        "When apitool starts, then this often",
        "apitool 啟動時同步一次，之後依此間隔",
    ),
    ("Whole word", "全字相符"),
    ("Will message", "遺囑訊息內容"),
    // MQTT 5 message and will properties.
    (
        "0: the broker keeps it until it's delivered",
        "0：broker 會一直保留到送達為止",
    ),
    ("Expires after", "到期時間"),
    ("Content type", "內容類型"),
    ("Response topic", "回覆主題"),
    (
        "Message properties need MQTT 5.0.",
        "訊息屬性需要 MQTT 5.0。",
    ),
    ("Use MQTT 5.0", "改用 MQTT 5.0"),
    (
        "What the payload is, for the receiver",
        "告訴接收端酬載的格式",
    ),
    (
        "Message properties need MQTT 5.0 (Settings)",
        "訊息屬性需要 MQTT 5.0（設定）",
    ),
    (
        "MQTT 5.0. Expiry 0: the broker keeps it until it's delivered",
        "MQTT 5.0。到期時間 0：broker 會一直保留到送達為止",
    ),
    ("Where the receiver should answer", "接收端應回覆到哪裡"),
    ("Will content type", "遺囑內容類型"),
    (
        "Windows has \"Automatically detect settings\" on: the PAC script is looked up via WPAD (DHCP, then DNS) before the first request. If what it finds isn't a usable script, requests go out directly.",
        "Windows 開啟了「自動偵測設定」：第一個請求送出前，會透過 WPAD（先 DHCP 後 DNS）尋找 PAC 指令碼。找到的若不是可用的指令碼，請求就直接連線。",
    ),
    (
        "Windows is configured with a PAC script, which will be used:\n{}",
        "Windows 已設定 PAC 指令碼，將會使用：\n{}",
    ),
    ("Workspace files changed", "工作區檔案已變更"),
    ("Workspace files not in step: {}", "工作區檔案未同步：{}"),
    ("Wrap", "自動換行"),
    ("Wrap long lines", "長行自動換行"),
    (
        "Written to {} in the workspace folder, for git.",
        "寫入工作區資料夾中的 {}，供 git 使用。",
    ),
    (
        "XPath: /a/b, //b, *, ., .., @attr, text(), [2], [last()], [@id], [@id='x'], [name='x'], [text()='x']; prefixes are ignored",
        "XPath：/a/b, //b, *, ., .., @attr, text(), [2], [last()], [@id], [@id='x'], [name='x'], [text()='x']；命名空間前綴會被忽略",
    ),
    (
        "{{private_key}} or -----BEGIN PRIVATE KEY-----…",
        "{{private_key}} 或 -----BEGIN PRIVATE KEY-----…",
    ),
    ("{} (default)", "{} (預設)"),
    ("{} (cut)", "{} (已截斷)"),
    ("{} added on Send", "傳送時加入 {} 個"),
    (
        "{} changed on disk; saving will overwrite that change",
        "{} 在磁碟上已變更；儲存會覆蓋該變更",
    ),
    ("{} d ago", "{} 天前"),
    ("{} failed requests", "{} 個請求失敗"),
    ("1 failed request", "1 個請求失敗"),
    ("{} h ago", "{} 小時前"),
    ("{} is available", "有新版本 {}"),
    (
        "{} is installed and starts next time",
        "{} 已安裝，下次啟動時生效",
    ),
    ("{} items", "{} 個項目"),
    (
        "{} jumps to any request or environment",
        "{} 可跳到任一請求或環境",
    ),
    ("{} min ago", "{} 分鐘前"),
    (
        "{} of binary data, not shown ({}).",
        "{} 的二進位資料，未顯示（{}）。",
    ),
    (
        "{} tabs with unsaved edits left open",
        "{} 個分頁有未儲存的變更，保持開啟",
    ),
    (
        "{} was deleted on disk; Save recreates it",
        "{} 已從磁碟刪除；儲存會重新建立",
    ),
    (
        "{}. Not carried over as it was:\n{}",
        "{}。以下未能照原樣匯入：\n{}",
    ),
    ("{}/{} tests", "{}/{} 項測試"),
    ("{}: no data rows", "{}：沒有資料列"),
    (
        "{}\nOnly the first {} were kept, to save memory.",
        "{}\n為了節省記憶體，只保留前 {}。",
    ),
    (
        "{}Waiting (TTFB) {} ms{}\nDownload {} ms",
        "{}等待（TTFB）{} ms{}\n下載 {} ms",
    ),
    ("· {} events", "· {} 個事件"),
    ("· {} of {} events", "· {} 個事件（共 {} 個）"),
    ("… and {} more", "…還有 {} 個"),
    ("Start", "開始"),
    // Zoom and shortcut help
    ("Default text size", "預設文字大小"),
    ("Keyboard shortcuts", "鍵盤快速鍵"),
    ("Larger text", "放大文字"),
    ("Smaller text", "縮小文字"),
    // Runner options
    ("Keep variable values", "保留變數值"),
    (
        "Off, what the run sets still carries from request to request, but the environment and globals stay as they were",
        "關閉時，執行中設定的值仍會在請求之間傳遞，但環境與全域變數維持原樣",
    ),
    ("Run without stored cookies", "不使用已儲存的 Cookie 執行"),
    ("Start from an empty cookie jar", "從空的 Cookie 罐開始"),
    ("Save cookies after the run", "執行後儲存 Cookie"),
    (
        "Off, the cookie jar is left as it was before the run",
        "關閉時，Cookie 罐維持執行前的樣子",
    ),
    ("Save responses", "儲存回應"),
    (
        "Keep each response, with up to 32 KiB of its body, to open from the results",
        "保留每個回應（內容最多 32 KiB），可從結果中開啟",
    ),
    ("Run again", "再執行一次"),
    (
        "What these results ran: the same requests in the same order, with the same settings",
        "再執行一次這些結果所跑的內容：相同的請求、相同的順序與相同的設定",
    ),
    ("Show or hide the response", "顯示或隱藏回應"),
    // Socket.IO path and version
    ("Handshake path", "交握路徑"),
    (
        "Where the server mounts Socket.IO; the URL's path is still the namespace",
        "伺服器掛載 Socket.IO 的位置；URL 的路徑仍是命名空間",
    ),
    ("Client version", "用戶端版本"),
    ("Socket.IO 2.x servers", "適用 Socket.IO 2.x 伺服器"),
    (
        "Socket.IO 3.x and 4.x servers",
        "適用 Socket.IO 3.x 與 4.x 伺服器",
    ),
    (
        "v2 has no auth payload: a JSON body isn't sent. Put what the server checks in Params; they go on the handshake's query.",
        "v2 沒有驗證 payload：不會送出 JSON 內容。伺服器要檢查的值請放在「參數」，會附在交握的查詢字串上。",
    ),
    (
        "A JSON body is sent as the auth payload when joining the namespace.",
        "加入命名空間時，JSON 內容會作為驗證 payload 送出。",
    ),
    // Selection menu
    ("A variable needs a name", "變數需要名稱"),
    ("Cut", "剪下"),
    ("Decode URI component", "URI 元件解碼"),
    ("Encode URI component", "URI 元件編碼"),
    ("New variable", "新增變數"),
    ("New variable…", "新增變數…"),
    (
        "Not decoded: the selection isn't valid URI encoding",
        "未解碼：選取的文字不是有效的 URI 編碼",
    ),
    ("Paste", "貼上"),
    ("Select all", "全選"),
    (
        "Select text to set it as a variable",
        "選取文字即可設為變數",
    ),
    ("Set as variable in {}", "設為變數：{}"),
    ("Set {} in {}", "已將 {} 設定於 {}"),
    // Variables panel
    (
        "The nearest scope wins. Enter keeps an edited value, Esc puts it back.",
        "最近的範圍優先。修改後按 Enter 保存，按 Esc 還原。",
    ),
    ("Folders", "資料夾"),
    ("Set in Folder settings.", "於資料夾設定中修改。"),
    ("Overridden by {}", "被 {} 覆蓋"),
    // Default headers
    ("Change the defaults…", "修改預設值…"),
    (
        "User-Agent, Accept and Accept-Encoding for every request, in Network settings. A request's own header still wins.",
        "所有請求的 User-Agent、Accept 與 Accept-Encoding，在網路設定中修改。請求自己設的標頭仍然優先。",
    ),
    ("Default headers", "預設標頭"),
    (
        "Sent with every request that doesn't set its own.",
        "請求沒有自己設定時，一律送出這些標頭。",
    ),
    ("Empty sends none", "留空則不送出"),
    (
        "Empty still sends */*: the HTTP client always sends an Accept.",
        "留空仍會送出 */*：HTTP 用戶端一定會帶 Accept。",
    ),
    (
        "Off: no Accept-Encoding, and a compressed body is shown as it came.",
        "關閉：不送 Accept-Encoding，壓縮過的內容會照原樣顯示。",
    ),
    // Request settings
    ("Follow original HTTP method", "沿用原本的 HTTP 方法"),
    (
        "Keep the method and body on a 301, 302 or 303",
        "遇到 301、302 或 303 時保留原本的方法與內容",
    ),
    ("Follow Authorization header", "保留 Authorization 標頭"),
    (
        "Keep it when a redirect goes to another host",
        "重新導向到其他主機時仍然送出",
    ),
    (
        "Remove referer header on redirect",
        "重新導向時移除 Referer 標頭",
    ),
    (
        "Don't tell the next host which URL redirected",
        "不讓下一個主機知道是哪個網址轉過來的",
    ),
    ("Disabled TLS versions", "停用的 TLS 版本"),
    (
        "apitool's TLS (rustls) has only TLS 1.2 and 1.3: SSL, TLS 1.0 and 1.1 are never offered and can't be turned on.",
        "apitool 的 TLS（rustls）只有 TLS 1.2 與 1.3：SSL、TLS 1.0 與 1.1 一律不提供，也無法開啟。",
    ),
    (
        "No TLS version is left: https requests fail",
        "沒有可用的 TLS 版本：https 請求會失敗",
    ),
    (
        "Only for servers that mishandle one of them",
        "只在伺服器處理不好其中一個版本時使用",
    ),
    ("Cipher suites", "加密套件"),
    (
        "Offered in the order checked; none checked offers all",
        "依勾選順序提供；都不勾則全部提供",
    ),
    ("Not available here, skipped: {}", "這裡沒有，已略過：{}"),
    ("Enable strict HTTP parser", "啟用嚴格 HTTP 解析"),
    (
        "Off accepts responses with malformed headers",
        "關閉時接受標頭格式不正確的回應",
    ),
    ("All", "全部"),
    ("{} chosen", "已選 {} 個"),
    // Hawk, NTLM, EdgeGrid auth
    ("Hawk", "Hawk"),
    ("Akamai EdgeGrid", "Akamai EdgeGrid"),
    ("NTLM", "NTLM"),
    ("Auth ID", "驗證 ID"),
    ("Auth key", "驗證金鑰"),
    ("User", "使用者"),
    ("Nonce", "Nonce"),
    ("random on each Send", "每次傳送隨機產生"),
    ("Extra data", "額外資料（ext）"),
    ("App", "應用程式（app）"),
    ("Delegation", "委派（dlg）"),
    ("Timestamp", "時間戳記"),
    ("now on each Send", "每次傳送取當下時間"),
    ("Include payload hash", "納入內容雜湊"),
    ("Client token", "用戶端權杖"),
    ("Base URL", "基底網址"),
    (
        "optional: the host to sign, if not the URL's",
        "選填：要簽章的主機（若與網址不同）",
    ),
    ("Headers to sign", "要簽章的標頭"),
    ("Max body", "內容上限（位元組）"),
    ("Workstation", "工作站"),
    (
        "Each Send is signed over its method, URL and host with a fresh nonce and timestamp; the payload hash adds the body and its content type. A body streamed from a file isn't hashed.",
        "每次傳送都會以新的 nonce 與時間戳記，對方法、網址與主機簽章；內容雜湊會再納入內容與其 Content-Type。從檔案串流的內容不會納入雜湊。",
    ),
    (
        "Each Send is signed over its method, URL, the headers to sign and a POST's body (up to max body) with a fresh nonce and timestamp. The tokens and host are in your .edgerc.",
        "每次傳送都會以新的 nonce 與時間戳記，對方法、網址、要簽章的標頭與 POST 內容（最多到內容上限）簽章。權杖與主機都在你的 .edgerc 裡。",
    ),
    (
        "Send signs in with NTLMv2 over HTTP/1.1: a challenge and answer on one connection, so the body goes twice. Through a proxy only https:// URLs work (a plain HTTP proxy may switch connections), and servers that only offer Kerberos aren't supported.",
        "傳送時以 NTLMv2 透過 HTTP/1.1 登入：挑戰與回應走同一條連線，因此內容會送出兩次。經由代理伺服器時只有 https:// 網址可用（一般 HTTP 代理可能換連線），也不支援只提供 Kerberos 的伺服器。",
    ),
    // Connection settings of streams and gRPC
    ("Handshake timeout", "交握逾時"),
    ("Connect timeout", "連線逾時"),
    (
        "How long connecting may take; 0 uses the network settings' timeout",
        "連線最久可花多少時間；0 表示沿用網路設定的逾時",
    ),
    ("Response timeout", "回應逾時"),
    (
        "How long the server may take to start the stream; 0 waits as long as it takes",
        "伺服器開始傳送串流前最久等多少時間；0 表示一直等",
    ),
    ("Deadline", "期限"),
    (
        "The call fails past it; 0 uses the network settings' timeout, or none for a stream",
        "超過即呼叫失敗；0 表示沿用網路設定的逾時，串流則不設限",
    ),
    ("Maximum message size", "訊息大小上限"),
    ("Default ({} MB)", "預設 ({} MB)"),
    (
        "A larger message received is an error",
        "收到更大的訊息即視為錯誤",
    ),
    ("Include default values", "包含預設值欄位"),
    ("Reconnection attempts", "重新連線次數"),
    ("Never", "不重連"),
    (
        "In a row, when the connection ends without Disconnect",
        "連線不是按「中斷連線」而結束時，連續嘗試的次數",
    ),
    ("Reconnection interval", "重新連線間隔"),
    ("Show the secret", "顯示機密內容"),
    (
        "The token has expired; Send gets a new one.",
        "權杖已過期；傳送時會取得新的。",
    ),
    ("Token {}, expires in {} min", "權杖 {}，{} 分鐘後過期"),
    ("Forget", "清除"),
    (
        "The next Send signs in from the start",
        "下次傳送會從頭登入",
    ),
    ("Double-click to rename", "按兩下可重新命名"),
    ("Variable", "變數"),
    ("Close the runner", "關閉執行器"),
    ("Filter by name or URL", "依名稱或 URL 篩選"),
    ("Flags", "旗標"),
    ("Subscription", "訂閱"),
    ("Event", "事件"),
    (
        "chat; empty: the message names it",
        "chat；留空則由訊息指定",
    ),
    ("Ack", "要求確認"),
    (
        "Ask the server to acknowledge it: its answer shows as \"ack\" with the same number",
        "要求伺服器回覆確認：回覆會顯示為「ack」加上同一個編號",
    ),
    ("Its argument: JSON, or text", "事件的參數：JSON 或文字"),
    ("Connect first", "請先連線"),
    ("The operation to run", "要執行的操作"),
    ("Client authentication", "用戶端驗證方式"),
    ("In the body", "放在內容中"),
    ("Basic Auth header", "Basic Auth 標頭"),
    ("Secret is Base64", "秘密為 Base64 編碼"),
    ("Prefix", "前綴"),
    ("Parameter", "參數名稱"),
    ("Tests", "測試"),
    (
        "No tests ran. Add checks in the Asserts tab, or pm.test(…) in the Post-response script.",
        "沒有執行任何測試。可在「斷言」分頁加入檢查，或在「回應後」指令碼使用 pm.test(…)。",
    ),
    (
        "One field per line. A query with # comments is left as it is.",
        "每個欄位一行。含 # 註解的查詢不會變更。",
    ),
    ("+{} s after connecting", "連線後 +{} 秒"),
    ("Show secret values", "顯示秘密值"),
    (
        "For each request, unless its Settings tab sets its own",
        "套用到每個請求，除非請求的「設定」分頁另有設定",
    ),
    ("Copy the headers", "複製標頭"),
    ("Copied the timeline", "已複製時間軸"),
    ("Copied the headers", "已複製標頭"),
    ("Copied the body", "已複製內容"),
    ("Dismiss", "關閉訊息"),
    ("Repository", "儲存庫"),
    ("Passphrase", "密語"),
    ("Syncing…", "同步中…"),
    (
        "Fill in the repository and branch first",
        "請先填入儲存庫與分支",
    ),
    ("Paste a token first", "請先貼上權杖"),
    (
        "Set a passphrase first: what's ticked goes encrypted",
        "請先設定密語：勾選的項目會加密後才送出",
    ),
    (
        "Import: Postman, OpenAPI, Insomnia, HAR ({})",
        "匯入：Postman、OpenAPI、Insomnia、HAR（{}）",
    ),
    (
        "Sent without credentials, whatever the folders above set.",
        "不帶任何憑證送出，不論上層資料夾怎麼設定。",
    ),
    ("Session", "工作階段"),
    ("Click again to clear all", "再點一次即全部清除"),
    (
        "Delete all {} requests in History? This can't be undone.",
        "刪除歷史紀錄中全部 {} 筆請求？此動作無法復原。",
    ),
    ("Clear response history", "清除回應歷史紀錄"),
    ("Create", "建立"),
    ("Close the load test", "關閉負載測試"),
    ("Request failed: {}", "請求失敗：{}"),
    ("Metadata", "中繼資料"),
    (
        "Pick a method first (↻ asks the server for them)",
        "請先選擇方法（↻ 會向伺服器查詢）",
    ),
    ("Auth payload", "驗證酬載"),
    (
        "Press Connect to join the namespace, then send events: a name, then its argument. Scripts don't run for Socket.IO.",
        "按「連線」加入命名空間，再傳送事件：先寫名稱，再寫參數。Socket.IO 不執行腳本。",
    ),
    (
        "A subscription: press Connect to start it; each event shows here. Scripts don't run for subscriptions.",
        "這是訂閱：按「連線」開始，每個事件都會顯示在這裡。訂閱不執行腳本。",
    ),
    (
        "Show the fields proto3 leaves out at 0, \"\" or false",
        "顯示 proto3 因值為 0、\"\" 或 false 而省略的欄位",
    ),
    ("Capture", "擷取"),
    (
        "Capture traffic (HTTP or SOCKS proxy)",
        "擷取流量（HTTP 或 SOCKS 代理）",
    ),
    (
        "Capture what apps send through a local HTTP or SOCKS proxy",
        "透過本機 HTTP 或 SOCKS 代理擷取應用程式送出的請求",
    ),
    ("Capture: {}", "擷取：{}"),
    ("Capture stopped", "已停止擷取"),
    ("Capturing on {}", "擷取中：{}"),
    (
        "Capturing on {}, as the system proxy",
        "擷取中：{}，已設為系統代理",
    ),
    (
        "Port {} is taken: capturing on {} instead",
        "連接埠 {} 已被占用：改在 {} 擷取",
    ),
    (
        "Apps set to this port find the capture there after every start",
        "設定成這個連接埠的應用程式，每次開始擷取後都能在這裡找到它",
    ),
    ("Close the capture", "關閉擷取"),
    (
        "Stops capturing and drops what it captured",
        "停止擷取並捨棄已擷取的內容",
    ),
    ("Listening on {}", "監聽於 {}"),
    ("Copy the address", "複製位址"),
    ("Start capturing", "開始擷取"),
    (
        "Set {} as the HTTP or SOCKS proxy of an app or of the system: what it sends shows here, forwarded with apitool's network settings. HTTPS shows as tunnels, their host and size only.",
        "把 {} 設為應用程式或系統的 HTTP 或 SOCKS 代理：送出的請求會顯示在這裡，並以 apitool 的網路設定轉送。HTTPS 顯示為通道，只有主機與大小。",
    ),
    ("{} flows", "{} 筆流量"),
    ("Export as HAR…", "匯出為 HAR…"),
    ("Filter by URL or method", "依 URL 或方法篩選"),
    ("Flows", "流量"),
    ("Minify", "壓縮"),
    ("Other devices too", "也接受其他裝置"),
    (
        "Listens on every network interface, not only 127.0.0.1, so WSL, a VM or a phone can use it. Anyone who can reach this machine can then send through it; Windows may ask to let apitool accept connections.",
        "監聽所有網路介面而不只 127.0.0.1，讓 WSL、虛擬機或手機也能使用。任何連得到這台電腦的人都能透過它送出請求；Windows 可能會詢問是否允許 apitool 接受連線。",
    ),
    (
        "Stop first: it applies when capturing starts",
        "請先停止：這個設定在開始擷取時生效",
    ),
    ("this machine's address", "這台電腦的位址"),
    (
        "From another device or a VM: {}:{}. From WSL 2: the Windows host's address, which `ip route show default | cut -d' ' -f3` prints in WSL, port {}; with mirrored networking, 127.0.0.1 works as is.",
        "其他裝置或虛擬機：{}:{}。WSL 2：用 Windows 主機的位址（在 WSL 執行 `ip route show default | cut -d' ' -f3` 可得），埠 {}；若使用 mirrored 網路模式，直接用 127.0.0.1 即可。",
    ),
    ("Connections", "連線"),
    (
        "Every TCP and UDP connection the OS lists, by process: no proxy needed, no byte counts",
        "作業系統列出的每個 TCP 與 UDP 連線，依程序排列：不必經過代理，但沒有位元組數",
    ),
    ("{} connections", "{} 個連線"),
    (
        "Filter by process, address or state",
        "依程序、位址或狀態篩選",
    ),
    (
        "Only your own processes show: the others' need root.",
        "只顯示你自己的程序：其他程序的連線需要 root 權限。",
    ),
    ("Nothing captured yet.", "尚未擷取到任何內容。"),
    ("Open as request", "開成請求"),
    (
        "A new request from what was sent, its answer saved as an example",
        "依送出的內容建立新請求，回應存為範例",
    ),
    ("A tunnel has no request to open", "通道沒有可開啟的請求"),
    ("({} of binary data)", "（{} 二進位資料）"),
    ("(the first {} of {})", "（只保留前 {}，共 {}）"),
    ("{} sent, {} received", "送出 {}，收到 {}"),
    ("Decrypt HTTPS", "解密 HTTPS"),
    (
        "Reads HTTPS too: each host gets a certificate from apitool's CA, which apps must trust",
        "連 HTTPS 也讀取：每個主機改用 apitool CA 簽發的憑證，應用程式必須信任這個 CA",
    ),
    ("Export CA certificate…", "匯出 CA 憑證…"),
    ("Trusting apitool's CA", "信任 apitool 的 CA"),
    ("Command line", "命令列工具"),
    ("Phones", "手機"),
    (
        "Save it with Export CA certificate… first: the commands then name the file.",
        "先用「匯出 CA 憑證…」存檔，下面的指令就會帶入那個檔案。",
    ),
    (
        "apitool's own requests trust it once the system does, or as the Extra CA bundle in Settings › Network (empty that field to take it back).",
        "系統信任之後，apitool 自己的請求也會信任；或在 設定 › 網路 把它填進「額外的 CA 憑證」（清空該欄即移除）。",
    ),
    ("Copied the command", "已複製指令"),
    ("Make a new CA", "產生新的 CA"),
    ("Click again to replace the CA", "再點一次即換掉 CA"),
    (
        "If its key may have got out. Every app that trusts the old one stops connecting until it trusts the new one.",
        "私鑰可能外流時使用。信任舊 CA 的應用程式都會連不上，直到改信任新的為止。",
    ),
    ("Made a new CA", "已產生新的 CA"),
    (
        "This is a new CA. Take the old one back first (each tab shows how), then export and trust this one: both have the same name, so taking one back by name removes both.",
        "這是新的 CA。請先移除舊的（每個分頁都有移除方式），再匯出並信任這一張：兩張同名，按名稱移除會兩張一起刪掉。",
    ),
    (
        "For this user, in Safari, Chrome, Edge, apitool and most apps; macOS asks for your password:",
        "對目前使用者生效，Safari、Chrome、Edge、apitool 與大多數應用程式都會信任；macOS 會要求輸入密碼：",
    ),
    (
        "Or by hand: open the file in Keychain Access, double-click apitool capture CA, and under Trust set When using this certificate to Always Trust.",
        "或手動：用「鑰匙圈存取」打開檔案，雙擊 apitool capture CA，在「信任」把「使用此憑證時」設為「永遠信任」。",
    ),
    ("To take it back:", "日後要移除："),
    (
        "Or by hand: in Keychain Access, under login › Certificates, delete apitool capture CA.",
        "或手動：在「鑰匙圈存取」的 登入 › 憑證 中刪除 apitool capture CA。",
    ),
    (
        "To take it back: in the same Authorities list, pick apitool capture CA under apitool and Delete or Distrust…; set security.enterprise_roots.enabled back to false if you changed it.",
        "日後要移除：在同一個「憑證機構」清單中，選 apitool 底下的 apitool capture CA，按「刪除或不信任…」；若改過 security.enterprise_roots.enabled，把它設回 false。",
    ),
    (
        "To take them back in that shell (a new shell starts without them):",
        "要在同一個 shell 移除（新開的 shell 本來就沒有）：",
    ),
    (
        "To take it back: on iPhone and iPad, Settings › General › VPN & Device Management › apitool capture CA › Remove Profile; on Android, Settings › Security › Encryption & credentials › User credentials › apitool capture CA › Remove. Then set the Wi-Fi proxy back to Off.",
        "日後要移除：iPhone 與 iPad 到 設定 › 一般 › VPN 與裝置管理 › apitool capture CA › 移除描述檔；Android 到 設定 › 安全性 › 加密與憑證 › 使用者憑證 › apitool capture CA › 移除。最後把 Wi-Fi 代理設回「關閉」。",
    ),
    (
        "For this user, in Edge, Chrome, apitool and most apps, no administrator needed; Windows asks to confirm:",
        "對目前使用者生效，Edge、Chrome、apitool 與大多數應用程式都會信任，不需要系統管理員；Windows 會請你確認：",
    ),
    (
        "Or by hand: double-click the .crt file, Install Certificate…, Current User, then Place all certificates in the following store, Browse…, Trusted Root Certification Authorities. Left to pick the store, Windows puts it under Intermediate Certification Authorities, where it isn't trusted.",
        "或手動：雙擊 .crt 檔，「安裝憑證…」，選「目前使用者」，再選「將所有憑證放入以下的存放區」，「瀏覽…」，選「受信任的根憑證授權單位」。若讓 Windows 自動選擇存放區，它會放進「中繼憑證授權單位」，那裡的憑證不被信任。",
    ),
    (
        "If it went under Intermediate Certification Authorities, from there too:",
        "若它被放進了「中繼憑證授權單位」，也要從那裡移除：",
    ),
    (
        "Or by hand: run certmgr.msc, and under Trusted Root Certification Authorities › Certificates (and Intermediate Certification Authorities › Certificates) delete apitool capture CA.",
        "或手動：執行 certmgr.msc，在「受信任的根憑證授權單位 › 憑證」（以及「中繼憑證授權單位 › 憑證」）刪除 apitool capture CA。",
    ),
    (
        "Debian, Ubuntu, and Ubuntu in WSL:",
        "Debian、Ubuntu，以及 WSL 裡的 Ubuntu：",
    ),
    ("Fedora, RHEL, Arch:", "Fedora、RHEL、Arch："),
    (
        "Chrome and Chromium keep their own list on Linux (certutil is in libnss3-tools):",
        "Linux 上的 Chrome 與 Chromium 用自己的清單（certutil 在 libnss3-tools 套件）：",
    ),
    (
        "Firefox keeps its own list: Settings › Privacy & Security › Certificates › View Certificates… › Authorities › Import…, pick the file and tick Trust this CA to identify websites.",
        "Firefox 用自己的清單：設定 › 隱私權與安全性 › 憑證 › 檢視憑證… › 憑證機構 › 匯入…，選這個檔案並勾選「信任由此憑證機構來識別網站」。",
    ),
    (
        "On Windows and macOS it can use the system's instead: set this to true in about:config.",
        "在 Windows 與 macOS 上也可以改用系統的清單：在 about:config 把這項設為 true。",
    ),
    (
        "Many tools bring their own roots and don't read the system's proxy. In the shell to capture from:",
        "很多工具自帶根憑證，也不讀系統代理。在要擷取的那個 shell 裡：",
    ),
    (
        "NODE_EXTRA_CA_CERTS adds to Node's roots. REQUESTS_CA_BUNDLE (Python requests, pip) and SSL_CERT_FILE (OpenSSL: Python, Ruby, PHP) replace theirs, which is fine while everything goes through the capture.",
        "NODE_EXTRA_CA_CERTS 是加進 Node 的根憑證；REQUESTS_CA_BUNDLE（Python requests、pip）與 SSL_CERT_FILE（OpenSSL：Python、Ruby、PHP）則會取代原本的，所有流量都經過擷取時沒有問題。",
    ),
    (
        "Java, into the JDK's own list (its password is changeit):",
        "Java，加進 JDK 自己的清單（密碼是 changeit）：",
    ),
    (
        "Tick Other devices too before Start, then set the phone's Wi-Fi proxy to Manual: server {}, port {}.",
        "開始前勾選「也接受其他裝置」，再把手機 Wi-Fi 的代理設為手動：伺服器 {}，連接埠 {}。",
    ),
    (
        "Get the file onto the phone: AirDrop, mail or a chat to yourself.",
        "把檔案傳到手機：AirDrop、郵件，或傳訊息給自己。",
    ),
    (
        "iPhone and iPad: open it, install it in Settings › Profile Downloaded, then turn it on in Settings › General › About › Certificate Trust Settings.",
        "iPhone 與 iPad：打開檔案，到 設定 › 已下載描述檔 安裝，再到 設定 › 一般 › 關於本機 › 憑證信任設定 開啟。",
    ),
    (
        "Android: Settings › Security › Encryption & credentials › Install a certificate › CA certificate (names vary by maker). Chrome trusts it; since Android 7, other apps only if they opt in.",
        "Android：設定 › 安全性 › 加密與憑證 › 安裝憑證 › CA 憑證（名稱依廠牌而異）。Chrome 會信任；Android 7 起，其他 App 要自己選擇信任使用者憑證才會生效。",
    ),
    ("Use as the system proxy", "設為系統代理"),
    (
        "Points the system's HTTP and HTTPS proxy here; Stop or quitting puts back what was set",
        "把系統的 HTTP 與 HTTPS 代理指向這裡；停止擷取或結束 apitool 時會還原原本的設定",
    ),
    ("Start capturing first", "請先開始擷取"),
    ("The system proxy is back as it was", "系統代理已還原"),
    (
        "The system proxy couldn't be put back: {}",
        "無法還原系統代理：{}",
    ),
    ("Setting the system proxy: {}", "設定系統代理：{}"),
    ("The system proxy points at {}", "系統代理已指向 {}"),
    ("Rules ({} on)", "規則（{} 條啟用）"),
    ("Add rule", "新增規則"),
    ("On", "啟用"),
    ("URL, * for any part", "URL，* 代表任意部分"),
    ("Map Local", "對應本機檔案"),
    ("Map Remote", "改送其他伺服器"),
    ("Request header", "請求標頭"),
    ("Response header", "回應標頭"),
    ("Throttle", "限速"),
    ("Breakpoint", "中斷點"),
    ("Request", "請求"),
    ("File to answer with", "用來回應的檔案"),
    ("Value (empty: removed)", "值（留空即移除）"),
    ("Held before going on", "送出前先等待"),
    (
        "The answer at most this fast; 0, as fast as it comes",
        "回應最快這個速度；0 表示不限速",
    ),
    ("Map Local: answered from {}", "對應本機檔案：以 {} 回應"),
    ("Map Remote: sent to {}", "改送其他伺服器：送往 {}"),
    ("Request header {} removed", "已移除請求標頭 {}"),
    ("Request header set: {}: {}", "已設定請求標頭：{}: {}"),
    ("Response header {} removed", "已移除回應標頭 {}"),
    ("Response header set: {}: {}", "已設定回應標頭：{}: {}"),
    ("Throttled: held {} ms", "已限速：等待 {} 毫秒"),
    (
        "Throttled: held {} ms, at most {} KiB/s",
        "已限速：等待 {} 毫秒，最快 {} KiB/s",
    ),
    ("Held at a breakpoint", "曾在中斷點暫停"),
    ("Held at a breakpoint: the request", "在中斷點暫停：請求"),
    ("Held at a breakpoint: the answer", "在中斷點暫停：回應"),
    ("{} more waiting", "還有 {} 個在等待"),
    ("Continue", "繼續"),
    ("Abort", "中止"),
    ("The app gets a 502 instead", "應用程式改收到 502"),
    (
        "Edit it, then Continue: the first line, a header a line, a blank line, the body.",
        "編輯後按「繼續」：第一行、每行一個標頭、一個空行、內文。",
    ),
    (
        "Add it to the trusted roots of the system (Keychain Access, certmgr) or of the app",
        "把它加到系統（鑰匙圈存取、certmgr）或應用程式的受信任根憑證",
    ),
    (
        "Set {} as the HTTP or SOCKS proxy of an app or of the system: what it sends shows here, forwarded with apitool's network settings. HTTPS is decrypted: an app that doesn't trust apitool's CA fails to connect.",
        "把 {} 設為應用程式或系統的 HTTP 或 SOCKS 代理：送出的請求會顯示在這裡，並以 apitool 的網路設定轉送。HTTPS 會被解密：不信任 apitool CA 的應用程式將無法連線。",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `t("…")` in the sources has a translation, and every translation is still
    /// used: switching language leaves no wrapped text in English, and the table doesn't
    /// rot as text changes.
    #[test]
    fn every_wrapped_text_is_translated_and_every_translation_used() {
        let sources = [
            include_str!("app.rs"),
            include_str!("varedit.rs"),
            include_str!("appearance.rs"),
            include_str!("store.rs"),
            include_str!("sync.rs"),
            include_str!("net.rs"),
        ];
        let mut wrapped = std::collections::BTreeSet::new();
        for src in sources {
            // As the compiler reads it: a Windows checkout has CRLF, and `\` before CR
            // wouldn't read as a line continuation.
            let src = &src.replace("\r\n", "\n");
            let calls =
                ["t(", "tf(", "n_("].map(|f| src.match_indices(f).map(move |(i, _)| (i, f.len())));
            for (i, len) in calls.into_iter().flatten() {
                // Not `foo_t(` or `.t(`.
                let before = src[..i].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                    continue;
                }
                // rustfmt may break the line before a long literal.
                let Some(rest) = src[i + len..].trim_start().strip_prefix('"') else {
                    continue;
                };
                let mut end = 0;
                let bytes = rest.as_bytes();
                while bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                wrapped.insert(unescape(&rest[..end]));
            }
        }
        let missing: Vec<_> = (wrapped.iter())
            .filter(|w| !ZH_TW_MAP.contains_key(w.as_str()))
            .collect();
        assert!(missing.is_empty(), "untranslated: {missing:#?}");
        let unused: Vec<_> = (ZH_TW.iter())
            .filter(|(en, _)| !wrapped.contains(*en))
            .collect();
        assert!(unused.is_empty(), "no longer in the code: {unused:#?}");
    }

    /// A literal's value as the compiler sees it: `\n`, `\"`, `\\` and line continuations.
    fn unescape(raw: &str) -> String {
        let mut out = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\n') => chars = chars.as_str().trim_start().chars(),
                Some(c) => out.push(c),
                None => {}
            }
        }
        out
    }

    #[test]
    fn the_language_is_per_thread_and_english_by_default() {
        assert_eq!(t("Send"), "Send");
        set(Lang::ZhTw);
        assert_eq!(t("Send"), "傳送");
        std::thread::spawn(|| assert_eq!(t("Send"), "Send"))
            .join()
            .unwrap();
        assert_eq!(t("no such text"), "no such text");
        set(Lang::English);
    }
}
