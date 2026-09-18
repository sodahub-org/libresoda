//! CDP 签名页：Rust 原生驱动 Chromium，替代 Node 版 `signer-server.mjs`。
//!
//! 做法和 Meting-API / 旧版 Node signer 完全一致，只是把「Playwright 遥控」换成
//! Rust 直接说 CDP：
//!
//! 1. 内置一个 127.0.0.1 静态资产服务，吐 `security_host.html` / `bdms.js`；
//! 2. 每个 `sessionKey` 建一个独立 `BrowserContext`（独立 cookie jar + 设备身份）；
//! 3. 页面里跑官方 `bdms.js`，由 `window.__qishuiRequest` 发真实 XHR（带 a_bogus）；
//! 4. 响应回来后校验 `a_bogus` 必须 44 字符，再把整个上下文的 cookie 交给调用方。

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams};
use chromiumoxide::Page;
use futures::StreamExt;

use crate::error::{Result, SodaError};
use crate::soda::browser::{BrowserCookie, BrowserRequest, BrowserRequester, BrowserResponse};

const SECURITY_HOST_HTML: &str =
    include_str!("../../tools/qishui-signer/security/security_host.html");
const SDK_GLUE_JS: &str = include_str!("../../tools/qishui-signer/security/sdk-glue.js");
const BDMS_JS: &str = include_str!("../../tools/qishui-signer/security/bdms.js");
const REACT_JS: &str = include_str!("../../tools/qishui-signer/security/react.js");
const REACT_DOM_JS: &str = include_str!("../../tools/qishui-signer/security/react-dom.js");

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) SodaMusic/3.2.1 Chrome/136.0.7103.59 Electron/36.4.0 Safari/537.36";
const DEFAULT_SESSION_KEY: &str = "default";

/// Rust 版签名页。懒启动：第一次请求才拉起 Chromium。
pub struct CdpSigner {
    inner: OnceLock<Arc<Inner>>,
}

struct Inner {
    runtime: tokio::runtime::Runtime,
    port: u16,
    state: Mutex<State>,
}

struct State {
    browser: Option<Arc<Browser>>,
    sessions: HashMap<String, Session>,
}

struct Session {
    context: BrowserContextId,
    page: Page,
    #[allow(dead_code)]
    last_used: Instant,
}

impl CdpSigner {
    pub fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn inner(&self) -> Result<Arc<Inner>> {
        if let Some(inner) = self.inner.get() {
            return Ok(inner.clone());
        }
        let port = start_asset_server()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| SodaError::http(format!("创建 tokio 运行时失败: {err}")))?;
        let inner = Arc::new(Inner {
            runtime,
            port,
            state: Mutex::new(State {
                browser: None,
                sessions: HashMap::new(),
            }),
        });
        let _ = self.inner.set(inner.clone());
        Ok(inner)
    }

    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        let inner = self.inner()?;
        inner
            .runtime
            .block_on(async_request(inner.clone(), request))
    }

    fn close(&self, session_key: &str) -> Result<()> {
        let key = normalize_key(session_key);
        let Some(inner) = self.inner.get() else {
            return Ok(());
        };
        inner.runtime.block_on(async_close(inner.clone(), &key))
    }
}

impl Default for CdpSigner {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserRequester for CdpSigner {
    fn request(&self, request: &BrowserRequest) -> Result<BrowserResponse> {
        CdpSigner::request(self, request)
    }

    fn close_session(&self, session_key: &str) -> Result<()> {
        self.close(session_key)
    }

    fn name(&self) -> &'static str {
        "cdp-signer"
    }
}

fn normalize_key(session_key: &str) -> String {
    let key = session_key.trim();
    if key.is_empty() {
        DEFAULT_SESSION_KEY.to_string()
    } else {
        key.to_string()
    }
}

#[derive(serde::Deserialize)]
struct JsXhr {
    status: u16,
    body: String,
    #[serde(rename = "responseURL")]
    response_url: String,
    headers: String,
}

async fn async_request(inner: Arc<Inner>, request: &BrowserRequest) -> Result<BrowserResponse> {
    let session = ensure_session(inner.clone(), &normalize_key(&request.session_key)).await?;
    let spec = serde_json::json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "body": request.body,
        "timeout": 180_000,
    });
    let call = CallFunctionOnParams::builder()
        .function_declaration("async function (spec) { return await window.__qishuiRequest(spec) }")
        .argument(CallArgument::builder().value(spec).build())
        .build()
        .map_err(|err| SodaError::http(format!("构造调用失败: {err}")))?;
    let result = session
        .page
        .evaluate_function(call)
        .await
        .map_err(|err| SodaError::http(format!("签名页执行请求失败: {err}")))?;
    let xhr: JsXhr = result
        .into_value()
        .map_err(|err| SodaError::http(format!("签名页返回无法解析: {err}")))?;
    // 只有护照 API 才要求 a_bogus；confirmed 后跟随 redirect_url 只是为了
    // 把登录 cookie 落到本上下文，那一跳本来就没有签名。
    if request.url.contains("/passport/") {
        let a_bogus = a_bogus_of(&xhr.response_url);
        if a_bogus.len() != 44 {
            return Err(SodaError::http(format!(
                "汽水安全签名生成失败：a_bogus 缺失（{} 字符）：{a_bogus}；URL={}",
                a_bogus.len(),
                xhr.response_url
            )));
        }
    }
    // CDP 的 Network.getCookies 默认只返回「当前页面 URL」的 cookie；
    // 签名页本身跑在 127.0.0.1，必须显式按汽水域名查，才能拿到
    // 与 Playwright context.cookies() 等价的整份登录态。
    let cookie_params = GetCookiesParams::builder()
        .url("https://api.qishui.com")
        .url("https://bff-pc.qishui.com")
        .url("http://api.qishui.com")
        .url("http://bff-pc.qishui.com")
        .build();
    let cookies = session
        .page
        .execute(cookie_params)
        .await
        .map_err(|err| SodaError::http(format!("读取签名页 cookie 失败: {err}")))?
        .result
        .cookies
        .iter()
        .map(|cookie| BrowserCookie {
            name: cookie.name.clone(),
            value: cookie.value.clone(),
            domain: cookie.domain.clone(),
        })
        .collect();
    Ok(BrowserResponse {
        ok: true,
        status: xhr.status,
        body: xhr.body,
        response_url: xhr.response_url,
        headers: xhr.headers,
        cookies,
        error: String::new(),
    })
}

async fn async_close(inner: Arc<Inner>, key: &str) -> Result<()> {
    let session = {
        let mut state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        state.sessions.remove(key)
    };
    let Some(session) = session else {
        return Ok(());
    };
    let browser = {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        state.browser.clone()
    };
    if let Some(browser) = browser {
        browser
            .dispose_browser_context(session.context)
            .await
            .map_err(|err| SodaError::http(format!("关闭签名页上下文失败: {err}")))?;
    }
    Ok(())
}

async fn ensure_session(inner: Arc<Inner>, key: &str) -> Result<Session> {
    {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        if let Some(session) = state.sessions.get(key) {
            return Ok(Session {
                context: session.context.clone(),
                page: session.page.clone(),
                last_used: Instant::now(),
            });
        }
    }
    let browser = ensure_browser(inner.clone()).await?;
    let context = browser
        .create_browser_context(CreateBrowserContextParams::default())
        .await
        .map_err(|err| SodaError::http(format!("创建签名页上下文失败: {err}")))?;
    let target = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(context.clone())
        .build()
        .map_err(|err| SodaError::http(format!("创建签名页页面参数失败: {err}")))?;
    let page = browser
        .new_page(target)
        .await
        .map_err(|err| SodaError::http(format!("创建签名页失败: {err}")))?;
    page.set_user_agent(USER_AGENT)
        .await
        .map_err(|err| SodaError::http(format!("设置签名页 UA 失败: {err}")))?;
    page.add_init_script(init_script(inner.port))
        .await
        .map_err(|err| SodaError::http(format!("注入签名页脚本失败: {err}")))?;
    let navigation = NavigateParams::builder()
        .url(format!(
            "http://127.0.0.1:{}/security_host.html",
            inner.port
        ))
        .build()
        .map_err(|err| SodaError::http(format!("构造导航失败: {err}")))?;
    page.goto(navigation)
        .await
        .map_err(|err| SodaError::http(format!("打开签名页失败: {err}")))?;
    wait_ready(&page).await?;
    let session = Session {
        context,
        page,
        last_used: Instant::now(),
    };
    inner
        .state
        .lock()
        .map_err(|_| SodaError::http("签名页状态锁中毒"))?
        .sessions
        .insert(key.to_string(), session.clone_like());
    Ok(session)
}

impl Session {
    /// chromiumoxide 的 Page 是 Clone（内部是句柄），这里复制一份放进缓存。
    fn clone_like(&self) -> Session {
        Session {
            context: self.context.clone(),
            page: self.page.clone(),
            last_used: self.last_used,
        }
    }
}

async fn wait_ready(page: &Page) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let state = page
            .evaluate_expression("JSON.stringify({bdms: typeof window.bdms, request: typeof window.__qishuiRequest})")
            .await
            .map_err(|err| SodaError::http(format!("检查签名页状态失败: {err}")))?;
        let text: String = state
            .into_value()
            .map_err(|err| SodaError::http(format!("解析签名页状态失败: {err}")))?;
        if text.contains("\"bdms\":\"object\"") && text.contains("\"request\":\"function\"") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(SodaError::http(
        "汽水安全组件初始化超时（bdms / __qishuiRequest 未就绪）",
    ))
}

async fn ensure_browser(inner: Arc<Inner>) -> Result<Arc<Browser>> {
    {
        let state = inner
            .state
            .lock()
            .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
        if let Some(browser) = &state.browser {
            return Ok(browser.clone());
        }
    }
    let executable = chromium_executable()
        .ok_or_else(|| SodaError::http("未找到 Chrome/Chromium，请设置 QISHUI_CHROMIUM_PATH"))?;
    // Chrome 新版本要求 --disable-web-security 必须搭配非默认 user-data-dir，
    // 否则该开关会被静默忽略（Playwright 之所以能用，是因为它永远开临时 profile）。
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    let user_data_dir = std::env::temp_dir().join(format!("libresoda-cdp-{unique}"));
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|err| SodaError::http(format!("创建 Chromium 临时目录失败: {err}")))?;
    let config = BrowserConfig::builder()
        .chrome_executable(&executable)
        .user_data_dir(&user_data_dir)
        // 默认旧 headless：实测 new headless 下 --disable-web-security 会被忽略
        .no_sandbox()
        // 注意：chromiumoxide 的 arg() 会自动补 `--` 前缀，
        // 再传 "--disable-web-security" 会变成 "----disable-web-security" 而被忽略。
        .arg("disable-setuid-sandbox")
        .arg("disable-dev-shm-usage")
        .arg("disable-gpu")
        .arg("disable-extensions")
        .arg("disable-background-networking")
        .arg("disable-sync")
        .arg("disable-default-apps")
        .arg("disable-component-update")
        .arg(("renderer-process-limit", "1"))
        .arg("disable-web-security")
        .arg((
            "disable-features",
            "IsolateOrigins,site-per-process,BlockThirdPartyCookies,ThirdPartyStoragePartitioning",
        ))
        .arg("disable-third-party-cookies");
    let config = config.build().map_err(|err| {
        SodaError::http(format!(
            "构造 Chromium 启动配置失败（{}）: {err}",
            executable.display()
        ))
    })?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|err| SodaError::http(format!("启动 Chromium 失败: {err}")))?;
    let browser = Arc::new(browser);
    tokio::task::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });
    let mut state = inner
        .state
        .lock()
        .map_err(|_| SodaError::http("签名页状态锁中毒"))?;
    if let Some(existing) = &state.browser {
        return Ok(existing.clone());
    }
    state.browser = Some(browser.clone());
    Ok(browser)
}

/// 找一个可用的 Chromium 系浏览器：显式配置 > 系统安装位置 > PATH。
///
/// 不限 Chrome/Chromium —— Edge（Windows 自带）、Brave、Vivaldi、Opera 都是
/// 同一套 CDP，能用就行；用户机器上大概率至少有一个。
pub fn chromium_executable() -> Option<PathBuf> {
    for key in ["QISHUI_CHROMIUM_PATH", "CHROMIUM_PATH"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value.trim());
            if path.is_file() {
                return Some(path);
            }
        }
    }
    if let Some(path) = candidate_paths().into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    let path_var = std::env::var_os("PATH")?;
    find_in_path(&binary_names(), &path_var)
}

/// PATH 查找（单独抽出来便于测试）。
fn find_in_path(names: &[&str], path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn binary_names() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec![
            "chrome.exe",
            "msedge.exe",
            "brave.exe",
            "vivaldi.exe",
            "opera.exe",
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "microsoft-edge-stable",
            "brave-browser",
            "vivaldi",
            "opera",
        ]
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for root in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            let Some(base) = std::env::var_os(root) else {
                continue;
            };
            let base = PathBuf::from(base);
            paths.push(base.join("Google/Chrome/Application/chrome.exe"));
            paths.push(base.join("Microsoft/Edge/Application/msedge.exe"));
            paths.push(base.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
            paths.push(base.join("Vivaldi/Application/vivaldi.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut roots = vec![PathBuf::from("/Applications")];
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join("Applications"));
        }
        for root in roots {
            paths.push(root.join("Google Chrome.app/Contents/MacOS/Google Chrome"));
            paths.push(root.join("Chromium.app/Contents/MacOS/Chromium"));
            paths.push(root.join("Microsoft Edge.app/Contents/MacOS/Microsoft Edge"));
            paths.push(root.join("Brave Browser.app/Contents/MacOS/Brave Browser"));
            paths.push(root.join("Vivaldi.app/Contents/MacOS/Vivaldi"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for path in [
            "/usr/local/bin/chromium",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/microsoft-edge",
            "/usr/bin/brave-browser",
            "/usr/bin/vivaldi-stable",
            "/snap/bin/chromium",
            "/var/lib/flatpak/exports/bin/org.chromium.Chromium",
        ] {
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

fn a_bogus_of(url: &str) -> String {
    let raw = url
        .split_once("a_bogus=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or_default().to_string())
        .unwrap_or_default();
    percent_decode(&raw)
}

/// 极简 percent-decode：签名里的 `/`、`+` 会被编码成 %2F/%2B，
/// 不解码的话 44 字符会被误判成 46/48。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// sdk-glue 会动态注入官方 CDN 的 bdms.js；把它改写到本地资产服务。
fn init_script(port: u16) -> String {
    format!(
        r#"
(function () {{
  const descriptor = Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype, 'src')
  const original = document.createElement.bind(document)
  document.createElement = function (tag, ...rest) {{
    const element = original(tag, ...rest)
    if (String(tag).toLowerCase() !== 'script' || !descriptor) return element
    let source = ''
    Object.defineProperty(element, 'src', {{
      configurable: true,
      get: () => source,
      set: (value) => {{
        source = /bdms\.js(\?|$)/i.test(String(value))
          ? 'http://127.0.0.1:{port}/bdms.js'
          : String(value)
        descriptor.set.call(element, source)
      }},
    }})
    return element
  }}
}})()
"#
    )
}

/// 极简静态资产服务：只服务 5 个签名页文件，不引 HTTP 依赖。
fn start_asset_server() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| SodaError::http(format!("签名页资产服务启动失败: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| SodaError::http(format!("读取签名页端口失败: {err}")))?
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = serve_asset(stream);
        }
    });
    Ok(port)
}

fn serve_asset(mut stream: TcpStream) -> std::io::Result<()> {
    use std::io::{Read, Write};

    let mut buffer = [0u8; 4096];
    let read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or("/security_host.html")
        .split('?')
        .next()
        .unwrap_or("/security_host.html");
    let (content_type, body) = match path {
        "/" | "/security_host.html" => ("text/html; charset=utf-8", SECURITY_HOST_HTML),
        "/sdk-glue.js" => ("text/javascript; charset=utf-8", SDK_GLUE_JS),
        "/bdms.js" => ("text/javascript; charset=utf-8", BDMS_JS),
        "/react.js" => ("text/javascript; charset=utf-8", REACT_JS),
        "/react-dom.js" => ("text/javascript; charset=utf-8", REACT_DOM_JS),
        _ => ("text/plain; charset=utf-8", "not found"),
    };
    let status = if body == "not found" { "404" } else { "200" };
    let header = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bogus_length_survives_percent_encoding() {
        // 真实形态：/ 会被编码成 %2F，不解码会误判成 46 字符
        let url = "https://api.qishui.com/x?a_bogus=mj-DfOgSMsf1SzPeR7kw997pb%2Fy0YW4FgZEz-wPbZtq6&next=1";
        assert_eq!(a_bogus_of(url).len(), 44, "{}", a_bogus_of(url));
    }

    #[test]
    fn percent_decode_handles_plain_text() {
        assert_eq!(percent_decode("abc%2Fd%2B"), "abc/d+");
        assert_eq!(percent_decode("no-encoding"), "no-encoding");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn find_in_path_locates_executable() {
        let dir = std::env::temp_dir().join(format!("libresoda-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let name = if cfg!(windows) {
            "msedge.exe"
        } else {
            "microsoft-edge"
        };
        let fake = dir.join(name);
        std::fs::write(&fake, b"#!/bin/sh\n").expect("fake browser");
        let found = find_in_path(&[name], dir.as_os_str()).expect("should find");
        assert_eq!(found, fake);
        assert!(find_in_path(&["definitely-missing-browser"], dir.as_os_str()).is_none());
        let _ = std::fs::remove_file(&fake);
    }
}
