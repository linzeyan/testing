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
    ("(top level)", "（最上層）"),
    ("*.corp.com or host:8443", "*.corp.com 或 host:8443"),
    (
        "+ and # are for subscribing; publish to one topic",
        "+ 和 # 只用於訂閱；發布時請指定單一主題",
    ),
    ("+ Certificate for a host", "+ 為主機加入憑證"),
    ("+ File…", "+ 檔案…"),
    ("+ Topic", "+ 主題"),
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
        "A broker drops the older of two connections with the same ID",
        "同一個 ID 有兩條連線時，broker 會斷開較舊的那條",
    ),
    ("A folder can't go inside itself", "資料夾不能放進自己裡面"),
    (
        "a folder name can't end with .toml",
        "資料夾名稱不能以 .toml 結尾",
    ),
    (
        "A Postman collection (v2.1) or environment, or an OpenAPI 3 / Swagger 2 spec (JSON or YAML): choose the file, paste it or its path, or drop the file here. Nothing already here is replaced.",
        "Postman 集合（v2.1）或環境，或 OpenAPI 3 / Swagger 2 規格（JSON 或 YAML）：選擇檔案、貼上內容或路徑，或把檔案拖曳到這裡。現有的內容都不會被取代。",
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
    ("Binary", "二進位檔"),
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
    ("Check now", "立即檢查"),
    ("Check when apitool starts", "每次啟動 apitool 時檢查"),
    (
        "Checked in order before the one above",
        "依序比對，優先於上方的憑證",
    ),
    ("Choose a file", "選擇檔案"),
    ("Choose file…", "選擇檔案…"),
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
    ("Close All Tabs", "關閉所有分頁"),
    ("Close Other Tabs", "關閉其他分頁"),
    ("Close Tab", "關閉分頁"),
    ("Close tab", "關閉分頁"),
    ("Close Tabs to the Right", "關閉右側分頁"),
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
    ("Copied the docs as Markdown", "已將文件複製為 Markdown"),
    ("Copied the mock server URL", "已複製模擬伺服器 URL"),
    (
        "Copied {} requests as a Postman collection{}",
        "已將 {} 個請求複製為 Postman 集合{}",
    ),
    ("Copied {} snippet", "已複製 {} 程式碼片段"),
    ("Copy", "複製"),
    ("Copy as Postman collection", "複製為 Postman 集合"),
    ("Copy body", "複製內容"),
    ("Copy docs as Markdown", "複製文件為 Markdown"),
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
    ("Don't check", "不檢查"),
    (
        "Downloads it in the background; it starts next time",
        "在背景下載，下次啟動時生效",
    ),
    ("Duplicate", "複製一份"),
    ("Duplicate environment", "複製環境"),
    ("Duplicate Tab", "複製分頁"),
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
    ("Edit globals", "編輯全域變數"),
    ("Edit this environment's variables", "編輯此環境的變數"),
    ("Edit {}", "編輯 {}"),
    ("empty for two-legged", "two-legged 時留空"),
    ("End stream", "結束串流"),
    ("Enter to apply", "按 Enter 套用"),
    ("Environment", "環境"),
    ("Environment \"{}\" already exists", "環境「{}」已存在"),
    ("Environment: {}", "環境：{}"),
    ("Error", "錯誤"),
    ("Errors", "錯誤"),
    ("ETag from an earlier response", "先前回應的 ETag"),
    ("ETag to match", "要比對的 ETag"),
    ("Examples", "範例"),
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
        "Fetch the schema and click a field →\nor type a query here.",
        "取得 schema 後點選欄位 →\n或在這裡輸入查詢。",
    ),
    (
        "Fetch the schema to browse its queries and mutations.",
        "取得 schema 後即可瀏覽它的查詢與變更。",
    ),
    ("File not found: {}", "找不到檔案：{}"),
    ("File path", "檔案路徑"),
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
    ("Globals", "全域變數"),
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
    ("GraphQL", "GraphQL"),
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
        "Markdown, for the docs a folder's right-click menu copies (\"Copy docs as Markdown\").",
        "Markdown，用於資料夾右鍵選單複製的文件（「複製文件為 Markdown」）。",
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
    ("Next page", "下一頁"),
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
    (
        "No requests yet. Click {} to add one.",
        "還沒有請求。按 {} 新增一個。",
    ),
    (
        "No variables yet. Add some to an environment or to Globals.",
        "還沒有變數。可以加到環境或全域變數裡。",
    ),
    ("None", "無"),
    ("not a PDF it can read", "不是可讀取的 PDF"),
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
    ("Open in browser", "在瀏覽器開啟"),
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
    ("Previous page", "上一頁"),
    ("Private key", "私密金鑰"),
    ("Properties", "屬性"),
    ("Proto", "Proto"),
    (
        "protos/service.proto, or empty to ask the server",
        "protos/service.proto，留空則向伺服器查詢",
    ),
    ("Proxy", "代理伺服器"),
    ("Proxy and certificates…", "代理伺服器與憑證…"),
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
    ("Remove", "移除"),
    ("Rename", "重新命名"),
    ("Reopen Closed Tab", "重新開啟已關閉的分頁"),
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
    (
        "Requests run in the order shown on the left, with the selected environment.",
        "請求依左側順序、以選定的環境執行。",
    ),
    (
        "Requests set to \"Inherit from parent\" use this.",
        "設為「繼承上層」的請求會使用這個。",
    ),
    (
        "Requests you send show up here.",
        "送出的請求會出現在這裡。",
    ),
    ("Reset", "重設"),
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
        "Select a request on the left, or create one with {}.",
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
    ("Stop", "停止"),
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
        "The collection runner is still running; cancel it first.",
        "集合執行器仍在執行，請先取消。",
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
    ("The request isn't open any more", "這個請求已經沒有開啟"),
    (
        "The response beside the request instead of under it",
        "回應放在請求旁邊，而不是下方",
    ),
    (
        "The response was over {} and wasn't kept while the tab was in the background: send again to see it",
        "回應超過 {}，分頁在背景時沒有保留：請重新傳送以查看",
    ),
    (
        "The server has {} services (gRPC reflection)",
        "伺服器有 {} 個服務（gRPC reflection）",
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
    (
        "User properties need MQTT 5.0 (Settings)",
        "使用者屬性需要 MQTT 5.0（設定）",
    ),
    (
        "User properties need MQTT 5.0 (Settings).",
        "使用者屬性需要 MQTT 5.0（設定）。",
    ),
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
    ("Variables (JSON)", "變數 (JSON)"),
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
        "What this is for, when to use it, what comes back…",
        "用途、使用時機、會回傳什麼…",
    ),
    (
        "What this response's Set-Cookie headers set",
        "此回應的 Set-Cookie 標頭設定了什麼",
    ),
    (
        "What was sent, redirects, and what came back",
        "送出的內容、重新導向與收到的回應",
    ),
    ("Whole word", "全字相符"),
    ("Will message", "遺囑訊息內容"),
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
    ("{} (cut)", "{} (已截斷)"),
    ("{} added on Send", "傳送時加入 {} 個"),
    (
        "{} changed on disk; saving will overwrite that change",
        "{} 在磁碟上已變更；儲存會覆蓋該變更",
    ),
    ("{} d ago", "{} 天前"),
    ("{} failed requests", "{} 個請求失敗"),
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
        "{} of binary data, not shown ({}). Save… keeps it as received.",
        "{} 的二進位資料，未顯示（{}）。「儲存…」會依原樣保存。",
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
    ("… and {} more", "…還有 {} 個"),
    ("▶ Run", "▶ 執行"),
    ("▶ Start", "▶ 開始"),
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
