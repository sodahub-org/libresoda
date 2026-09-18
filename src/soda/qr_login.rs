//! 汽水扫码登录 —— 按 `qq01-hub/Meting-API` 的 `providers/qishui/qr.js` 复刻，
//! 并对实测证伪的两处做了修正（见 3、5）。
//!
//! 复刻要点（全部来自参考实现，规格见 `~/Work/soda-sourcemap/docs/METING-QR-LOGIN-SPEC.md`）：
//!
//! 1. 每个二维码一份会话：随机 `device_id`(16 位数字)、`install_id`(15 位)、
//!    `msToken`(88 字节 base64url + `==`)、`verify_portrait_id`(`<uuid>.login`)、
//!    每请求随机的 `biz_trace_id`(8 hex)；
//! 2. 请求带完整 JS-SDK 公共参数 + `x-tt-passport-verify-portrait` /
//!    `x-tt-passport-trace-id`；POST 另带 `x-ss-stub = MD5(body).toUpperCase()`；
//! 3. **二维码内容必须是服务端下发的 `qrcode_index_url` 原样**（`ucenter_web/app/
//!    sdk-next?...&uc_sdk=scan-auth`）。参考实现改写成了 `light/invoke/scan_login`，
//!    实测手机扫改写版只会上报「已扫码」，点确认后服务端永远停在 `scanned`；
//!    扫原样地址才会走到 `confirmed`（`qrcode_index_url` 用浏览器直接打开确实 404，
//!    但那是网页行为，跟 App 扫码无关）；
//! 4. 轮询 `POST /passport/web/check_qrconnect/`，`error_code=7` 只当**临时限流**
//!    （冷却 5s、建议 60s 后重试，不判失败）；`2`/`expired` 才是过期；`2046` 是二次验证；
//! 5. 成功后的登录态**不在响应体里**，而在「签名页浏览器会话」的 cookie jar 中：
//!    确认后跟一跳 `redirect_url` 把 cookie 引出来，再校验
//!    `sessionid|sessionid_ss|sid_guard|sid_tt`。因此登录请求必须经签名页
//!    （`a_bogus`）发出 —— 实测本地直连会一路 `error_code=7`。

use super::Soda;
use crate::error::{Result, SodaError};
use crate::model::{QRLoginResult, QRLoginStatus};
use crate::util::{now_millis, Params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const API_BASE: &str = "https://api.qishui.com";
const AID: &str = "386088";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) SodaMusic/3.2.1 Chrome/136.0.7103.59 Electron/36.4.0 Safari/537.36";
const SESSION_TTL_MS: i64 = 3 * 60 * 1000;
const MIN_CHECK_INTERVAL_MS: i64 = 2_500;
/// 服务端限流（`error_code=7`）后的冷却时间。
///
/// 服务端返回的文案是「访问太频繁，请稍后再试」，建议 60 秒；官方客户端在限流时也会
/// 放缓轮询（1s → 3s → 5s）。这里按服务端建议取 60 秒：冷却期内重复调用直接返回上次
/// 结果，不再打服务端，避免把 IP 拖进更长的限流。
/// `error_code=7` 的内部冷却：只压 5 秒（对齐上游 `session.cooldownUntil = now+5s`）。
///
/// 注意别写成 60 秒：`retry_after_ms=60000` 只是给客户端的**提示**，真按 60 秒冷却
/// 会把「扫码后确认」的窗口整段跳过，表现为一直卡在 `scanned`（实测踩过）。
const RATE_LIMIT_COOLDOWN_MS: i64 = 5_000;

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrSession {
    pub token: String,
    /// 签名页（浏览器）里的会话隔离键：一个二维码对应一个独立浏览器上下文。
    ///
    /// 对齐 Meting-API `signer.js` 的 `sessionKey`：共用一个浏览器上下文时，
    /// 多个二维码会话会共享同一套设备身份，很快被护照服务限流（`error_code=7`）。
    /// 老版本存的状态文件没有这个字段，反序列化时补空串（会退化成共用一个上下文）。
    #[serde(default)]
    pub session_key: String,
    pub device_id: String,
    pub install_id: String,
    pub ms_token: String,
    pub verify_portrait_id: String,
    pub cookie: String,
    pub created_ms: i64,
    pub last_check_ms: i64,
    pub cooldown_until_ms: i64,
    pub last_result: Option<QRLoginResult>,
}

/// 会话持久化路径：设置 `SODA_QR_STATE` 后，创建与轮询可以分两次进程完成。
fn state_path() -> Option<PathBuf> {
    std::env::var("SODA_QR_STATE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn save_state(session: &QrSession) {
    let Some(path) = state_path() else {
        return;
    };
    if let Ok(text) = serde_json::to_string(session) {
        let _ = std::fs::write(path, text);
    }
}

fn load_state(token: &str) -> Option<QrSession> {
    let path = state_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let session: QrSession = serde_json::from_str(&text).ok()?;
    (session.token == token.trim()).then_some(session)
}

fn sessions() -> &'static Mutex<HashMap<String, QrSession>> {
    static MAP: OnceLock<Mutex<HashMap<String, QrSession>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cleanup_sessions() {
    let now = now_millis();
    if let Ok(mut map) = sessions().lock() {
        map.retain(|_, session| now - session.created_ms < SESSION_TTL_MS);
    }
}

fn session_get(token: &str) -> Option<QrSession> {
    cleanup_sessions();
    sessions()
        .lock()
        .ok()
        .and_then(|map| map.get(token.trim()).cloned())
}

fn session_save(session: &QrSession) {
    if let Ok(mut map) = sessions().lock() {
        map.insert(session.token.clone(), session.clone());
    }
}

fn session_remove(token: &str) {
    if let Ok(mut map) = sessions().lock() {
        map.remove(token.trim());
    }
}

// ---------------------------------------------------------------------------
// 随机数（/dev/urandom 播种的 xorshift，避免引入 rand 依赖）
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = random_seed();
        let mut value = u64::from_le_bytes([
            seed[0], seed[1], seed[2], seed[3], seed[4], seed[5], seed[6], seed[7],
        ]) ^ now_millis() as u64;
        if value == 0 {
            value = 0x9E37_79B9_7F4A_7C15;
        }
        Rng(value)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }

    /// 小写十六进制串（用于浏览器会话隔离键）。
    fn hex(&mut self, bytes: usize) -> String {
        let mut out = String::with_capacity(bytes * 2);
        for _ in 0..bytes {
            out.push_str(&format!("{:02x}", self.next_byte()));
        }
        out
    }

    /// 首位 1-8、其余 0-9（等价参考实现的 randomDigits）。
    fn digits(&mut self, length: usize) -> String {
        let mut out = String::with_capacity(length);
        out.push(
            ((self.next_byte() % 8) + 1)
                .to_string()
                .chars()
                .next()
                .unwrap(),
        );
        while out.len() < length {
            out.push(((self.next_byte() % 10) + 48) as char);
        }
        out
    }

    /// 与上游参考实现一致的 UUID（hex 形态，8-4-4-4-12）。
    fn uuid(&mut self) -> String {
        let mut bytes = [0u8; 16];
        for slot in bytes.iter_mut() {
            *slot = self.next_byte();
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }
}

/// 取 16 字节随机种子。
///
/// ⚠️ **不要**用 `std::fs::read("/dev/urandom")`：Linux 的 `/dev/urandom` 是无限流，
/// `fs::read` 会一直读到内存耗尽（本项目踩过这个坑：直接把整机拖到 OOM/卡死）。
/// 这里只读固定的 16 字节，读不到时退化为时间戳。
fn random_seed() -> [u8; 16] {
    random_bytes::<16>()
}

/// 读 N 字节系统随机数（**有界**读取；禁止对 `/dev/urandom` 用 `fs::read`）。
fn random_bytes<const N: usize>() -> [u8; N] {
    use std::io::Read;

    let mut out = [0u8; N];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    // 退路：纳秒时间戳（仅在 /dev/urandom 不可用时发生）
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = ((nanos >> ((index % 16) * 8)) & 0xFF) as u8 ^ (index as u8);
    }
    out
}

fn random_ms_token() -> String {
    // 形态对齐上游参考实现 `Meting-API/src/providers/qishui/qr.js`：
    // `msToken = ${randomBase64Url(88)}==`（88 字节 → base64url 无填充 + "=="）。
    let bytes = random_bytes::<88>();
    format!("{}==", base64_url_no_pad(&bytes))
}

fn base64_url_no_pad(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

/// 参考实现用 8 位随机 hex 作为 `biz_trace_id`。
fn random_biz_trace_id() -> String {
    let mut rng = Rng::new();
    let mut out = String::with_capacity(8);
    for _ in 0..4 {
        out.push_str(&format!("{:02x}", rng.next_byte()));
    }
    out
}

// ---------------------------------------------------------------------------
// MD5（用于 `x-ss-stub`，自实现以避免新增依赖）
// ---------------------------------------------------------------------------

pub fn md5_hex_upper(input: &[u8]) -> String {
    let mut k = [0u32; 64];
    for (index, item) in k.iter_mut().enumerate() {
        let value = ((index + 1) as f64).sin().abs();
        *item = (value * 4_294_967_296.0) as u32;
    }
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    let mut message = input.to_vec();
    let bit_len = (input.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    for chunk in message.chunks(64) {
        let mut m = [0u32; 16];
        for (index, slot) in m.iter_mut().enumerate() {
            *slot = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 公共参数与请求
// ---------------------------------------------------------------------------

fn common_params(session: &QrSession, biz_trace_id: &str) -> Params {
    let mut params = Params::new();
    for (key, value) in [
        ("passport_jssdk_version", "2.4.13"),
        ("passport_jssdk_type", "normal"),
        ("is_from_ttaccountsdk", "1"),
        ("aid", AID),
        ("language", "zh"),
        ("account_sdk_source", "web"),
        ("p_js_v", "2.4.13"),
        ("p_js_t", "pro"),
        ("p_zt", "3.3.5"),
        ("p_ver", "1.0.29"),
        ("request_host", "app%3A%2F%2Fresources"),
        ("p_bd", "1.0.0.41"),
        ("is_new_login", "1"),
        ("is_from_iesaccountsaas", "1"),
        ("device_platform", "PC"),
        ("version_code", "3.5.2"),
        ("account_sdk_source_info", "00"),
    ] {
        params.set(key, value);
    }
    params.set("biz_trace_id", biz_trace_id);
    params.set("device_id", session.device_id.clone());
    params.set("install_id", session.install_id.clone());
    params.set("did", session.device_id.clone());
    params.set("iid", session.install_id.clone());
    params.set("msToken", session.ms_token.clone());
    params
}

fn build_url(path: &str, params: &Params) -> String {
    format!("{API_BASE}{path}?{}", params.encode())
}

fn merge_cookies(current: &str, values: &[String]) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut push = |name: String, value: String| {
        if name.is_empty() || value.is_empty() {
            return;
        }
        if let Some(slot) = pairs.iter_mut().find(|(key, _)| *key == name) {
            slot.1 = value;
        } else {
            pairs.push((name, value));
        }
    };
    for raw in std::iter::once(current.to_string()).chain(values.iter().cloned()) {
        for part in raw.split([';', ',']) {
            let Some((name, rest)) = part.split_once('=') else {
                continue;
            };
            let name = name.trim().to_string();
            let value = rest
                .trim()
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if matches!(
                name.to_lowercase().as_str(),
                "path" | "domain" | "expires" | "max-age" | "samesite" | "secure" | "httponly"
            ) {
                continue;
            }
            push(name, value);
        }
    }
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn response_cookie_pairs(cookies: &BTreeMap<String, String>) -> Vec<String> {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

/// 等价参考实现的 `requestPassport`。
fn request_passport(
    soda: &Soda,
    session: &mut QrSession,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body_values: Option<&Params>,
) -> Result<Value> {
    let biz_trace_id = random_biz_trace_id();
    let mut params = common_params(session, &biz_trace_id);
    for (key, value) in extra {
        params.set(*key, *value);
    }
    let url = build_url(path, &params);

    let body = body_values.map(|values| values.encode());
    let mut options = crate::http::RequestOption::new()
        .header("Accept", "application/json, text/javascript")
        .header("User-Agent", USER_AGENT)
        .header(
            "x-tt-passport-verify-portrait",
            session.verify_portrait_id.clone(),
        )
        .header("x-tt-passport-trace-id", biz_trace_id.clone());
    if let Some(body) = &body {
        options = options
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("x-ss-stub", md5_hex_upper(body.as_bytes()));
    }
    if !session.cookie.is_empty() {
        options = options.header("Cookie", session.cookie.clone());
    }

    // 这里**不**注入应用级签名（libmssdk 的 x-helios / x-medusa）：
    // Passport 的公共参数里带着本会话自己的 device_id，套上另一套设备指纹的签名
    // 反而会让服务端把请求判成异常设备。上游 Meting-API 同样只带
    // `x-tt-passport-*` 头，签名由签名页补齐（a_bogus / X-Helios / X-Medusa）。

    // 配了浏览器请求器时，请求交给签名页面发出（自带 a_bogus / X-Helios / X-Medusa）
    if let Some(requester) = soda.browser_requester() {
        let mut headers = BTreeMap::new();
        for (key, value) in options.headers() {
            headers.insert(key.clone(), value.clone());
        }
        let request = crate::soda::browser::BrowserRequest {
            session_key: session.session_key.clone(),
            method: method.to_string(),
            url: url.clone(),
            headers,
            body: body.clone(),
            ms_token: session.ms_token.clone(),
        };
        let response = requester.request(&request)?;
        session.cookie = merge_cookies(&session.cookie, &response.cookie_pairs());
        return serde_json::from_str(&response.body)
            .map_err(|err| SodaError::json(format!("汽水登录接口返回无效数据: {err}")));
    }

    let response = if method == "POST" {
        crate::http::post_bytes(&url, body.as_deref().unwrap_or("").as_bytes(), &[options])?
    } else {
        crate::http::get_full(&url, &[options])?
    };
    let cookie_pairs = response_cookie_pairs(&response.cookies);
    session.cookie = merge_cookies(&session.cookie, &cookie_pairs);

    if std::env::var("SODA_QR_DUMP").ok().as_deref() == Some("1") {
        // 只打 cookie 名字与数量，值一律脱敏
        let names: Vec<&str> = session
            .cookie
            .split(';')
            .filter_map(|pair| pair.trim().split_once('=').map(|(name, _)| name))
            .collect();
        let received: Vec<&str> = cookie_pairs
            .iter()
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name))
            .collect();
        eprintln!(
            "[qr-dump/cookie] {path} 本次收到={received:?} 会话累计={names:?} 长度={}",
            session.cookie.len()
        );
    }

    serde_json::from_slice(&response.body)
        .map_err(|err| SodaError::json(format!("汽水登录接口返回无效数据: {err}")))
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 是否已经拿到登录态：按 **cookie 名**精确判定（`sessionid` 不能按子串匹配，
/// 否则 `sessionid_ss=` 之外的字段会误判）。
fn has_login_state(cookie: &str) -> bool {
    const NAMES: [&str; 4] = ["sessionid", "sessionid_ss", "sid_guard", "sid_tt"];
    cookie.split(';').any(|pair| {
        pair.trim()
            .split_once('=')
            .map(|(name, value)| NAMES.contains(&name.trim()) && !value.trim().is_empty())
            .unwrap_or(false)
    })
}

/// 跟着 `redirect_url` 走一跳，把签名页浏览器会话里的登录 cookie 引到本地会话。
///
/// 官方在 `check_qrconnect` 确认后并不回传 `session_cookie`，登录态是随
/// 「同一浏览器会话」的 cookie jar 下发的；所以这一跳必须由签名页发出（带
/// `a_bogus` 且 `withCredentials`），本地直连拿不到。
fn harvest_cookies(soda: &Soda, session: &mut QrSession, url: &str) -> Result<()> {
    if let Some(requester) = soda.browser_requester() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Accept".to_string(),
            "application/json, text/javascript".to_string(),
        );
        headers.insert("User-Agent".to_string(), USER_AGENT.to_string());
        if !session.cookie.is_empty() {
            headers.insert("Cookie".to_string(), session.cookie.clone());
        }
        let request = crate::soda::browser::BrowserRequest {
            session_key: session.session_key.clone(),
            method: "GET".to_string(),
            url: url.to_string(),
            headers,
            body: None,
            ms_token: session.ms_token.clone(),
        };
        let response = requester.request(&request)?;
        session.cookie = merge_cookies(&session.cookie, &response.cookie_pairs());
        return Ok(());
    }

    let mut options = crate::http::RequestOption::new().header("User-Agent", USER_AGENT);
    if !session.cookie.is_empty() {
        options = options.header("Cookie", session.cookie.clone());
    }
    let response = crate::http::get_full(url, &[options])?;
    let pairs = response_cookie_pairs(&response.cookies);
    session.cookie = merge_cookies(&session.cookie, &pairs);
    Ok(())
}

/// 关闭该会话在签名页里的浏览器上下文（尽力而为：失败不影响登录结果，
/// 签名服务也有 5 分钟空闲回收兜底）。
fn close_browser_session(soda: &Soda, session: &QrSession) {
    if session.session_key.trim().is_empty() {
        return;
    }
    if let Some(requester) = soda.browser_requester() {
        let _ = requester.close_session(&session.session_key);
    }
}

/// 创建二维码（等价 `createQishuiQr`）。
#[derive(Debug, Clone, Default)]
pub struct QrCreateResult {
    pub token: String,
    /// 给 App 扫的地址：**服务端下发的 `qrcode_index_url` 原样**。
    ///
    /// 不要改写成 `light/invoke/scan_login`：实测官方 App 扫改写后的地址只会
    /// 上报「已扫码」，点确认后服务端状态永远停在 `scanned`；扫描原样地址才能
    /// 走到 `confirmed`。
    pub scan_url: String,
    /// 服务端下发的二维码图片（data URL）
    pub qr_image: String,
    pub expire_time: i64,
}

pub fn create_qr(soda: &Soda) -> Result<QrCreateResult> {
    cleanup_sessions();
    let mut rng = Rng::new();
    let mut session = QrSession {
        token: String::new(),
        // 每个二维码一个独立浏览器上下文（对齐 Meting-API 的 sessionKey）
        session_key: format!("qr-{}", rng.hex(16)),
        device_id: rng.digits(16),
        install_id: rng.digits(15),
        ms_token: random_ms_token(),
        verify_portrait_id: format!("{}.login", rng.uuid()),
        cookie: String::new(),
        created_ms: now_millis(),
        last_check_ms: 0,
        cooldown_until_ms: 0,
        last_result: None,
    };

    let payload = request_passport(
        soda,
        &mut session,
        "GET",
        "/passport/web/get_qrcode/",
        &[
            ("next", API_BASE),
            ("need_logo", "false"),
            ("need_short_url", "false"),
            ("is_new_login", "1"),
        ],
        None,
    )?;

    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    if std::env::var("SODA_QR_DUMP").ok().as_deref() == Some("1") {
        eprintln!(
            "[qr-dump/create] keys={:?} is_frontier={:?} expire_time={:?} status={:?}",
            data.as_object()
                .map(|map| map.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            data.get("is_frontier"),
            data.get("expire_time"),
            data.get("status"),
        );
    }
    let token = data
        .get("token")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let error_code = data
        .get("error_code")
        .and_then(|item| item.as_i64())
        .unwrap_or(0);
    let message = payload
        .get("message")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string();
    let description = data
        .get("description")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string();
    if message != "success" || error_code != 0 || token.is_empty() {
        return Err(SodaError::not_found(if description.is_empty() {
            "汽水二维码生成失败".to_string()
        } else {
            description
        }));
    }

    session.token = token.clone();
    let index_url = data
        .get("qrcode_index_url")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string();
    let scan_url = official_scan_url(&index_url)?;
    session_save(&session);
    save_state(&session);

    Ok(QrCreateResult {
        token,
        scan_url,
        qr_image: data
            .get("qrcode")
            .and_then(|item| item.as_str())
            .unwrap_or_default()
            .to_string(),
        expire_time: data
            .get("expire_time")
            .and_then(|item| item.as_i64())
            .unwrap_or(0),
    })
}

/// 测试用：暴露 base64url 编码（不带 padding）。
#[doc(hidden)]
pub fn base64_url_no_pad_for_test(data: &[u8]) -> String {
    base64_url_no_pad(data)
}

/// 二维码内容 = 服务端下发的 `qrcode_index_url` 原样返回。
///
/// 等价上游 `Meting-API/src/providers/qishui/qr.js` 的 `officialScanUrl`，但**不做**
/// 那层 `light/invoke/scan_login` 改写 —— 实测改写版二维码只能让手机上报
/// 「已扫码」，点确认之后状态永远停在 `scanned`（扫原样地址才能到 `confirmed`）。
pub fn official_scan_url(index_url: &str) -> Result<String> {
    let url = index_url.trim();
    if url.is_empty() {
        return Err(SodaError::invalid_input("汽水二维码缺少扫码地址"));
    }
    let has_token = url
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .any(|(key, value)| key == "token" && !value.is_empty())
        })
        .unwrap_or(false);
    if !has_token {
        return Err(SodaError::invalid_input("汽水二维码缺少登录 token"));
    }
    Ok(url.to_string())
}

/// 轮询扫码状态（等价 `checkQishuiQr`）。
pub fn check_qr(soda: &Soda, token: &str) -> Result<QRLoginResult> {
    let token = token.trim();
    let mut session = match session_get(token).or_else(|| load_state(token)) {
        Some(session) => session,
        None => return Err(SodaError::not_found("汽水二维码会话已过期，请重新生成")),
    };
    let now = now_millis();
    if session.cooldown_until_ms > now {
        if let Some(last) = session.last_result.clone() {
            return Ok(last);
        }
    }
    if session.last_result.is_some() && now - session.last_check_ms < MIN_CHECK_INTERVAL_MS {
        if let Some(last) = session.last_result.clone() {
            return Ok(last);
        }
    }

    let mut body = Params::new();
    body.set("need_logo", "false");
    body.set("need_short_url", "false");
    body.set("is_frontier", "true");
    body.set("token", token);
    body.set("is_new_login", "1");
    body.set("next", API_BASE);

    let payload = request_passport(
        soda,
        &mut session,
        "POST",
        "/passport/web/check_qrconnect/",
        &[],
        Some(&body),
    )?;

    // 排查用：`SODA_QR_DUMP=1` 时打印**脱敏**的回包结构（只打状态字段与键名，
    // 不含 cookie / sessionid / 签名值）。
    if std::env::var("SODA_QR_DUMP").ok().as_deref() == Some("1") {
        let data = payload.get("data").cloned().unwrap_or(Value::Null);
        let keys: Vec<String> = data
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        let has_session_material = ["session_cookie", "auth", "cookie", "sessionid"]
            .iter()
            .map(|key| {
                format!(
                    "{key}={}",
                    data.get(key).is_some() || payload.get(key).is_some()
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "[qr-dump] message={} keys={keys:?} status={} error_code={} description={:?} {has_session_material}",
            payload.get("message").and_then(|v| v.as_str()).unwrap_or(""),
            data.get("status").map(|v| v.to_string()).unwrap_or_default(),
            data.get("error_code").map(|v| v.to_string()).unwrap_or_default(),
            data.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        );
    }

    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let error_code = data
        .get("error_code")
        .and_then(|item| item.as_i64())
        .unwrap_or(0);
    let raw_status = data
        .get("status")
        .map(|item| match item {
            Value::String(text) => text.trim().to_lowercase(),
            Value::Number(number) => number.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let description = data
        .get("description")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut extra: BTreeMap<String, String> = BTreeMap::new();
    extra.insert("error_code".to_string(), error_code.to_string());
    if !raw_status.is_empty() {
        extra.insert("api_status".to_string(), raw_status.clone());
    }
    if !description.is_empty() {
        extra.insert("description".to_string(), description.clone());
    }

    let response_session_id = payload
        .get("data")
        .and_then(|item| item.get("sessionid"))
        .and_then(|item| item.as_str())
        .map(|value| format!("sessionid={value}"));
    if let Some(pair) = response_session_id {
        session.cookie = merge_cookies(&session.cookie, &[pair]);
    }

    // 只看服务端状态判断是否确认。**不能**用「本地会话里已经有 sessionid」来判：
    // 签名页是共享的浏览器上下文，jar 里可能还留着上一次登录的 cookie，那样第一次
    // 轮询就会误判成登录成功（实测踩过）。jar 只在确认之后用来取 cookie 值。
    let confirmed = matches!(raw_status.as_str(), "3" | "confirmed" | "success")
        || data.get("logged_in").and_then(|item| item.as_bool()) == Some(true)
        || data.get("session_cookie").is_some();

    let build =
        |status: QRLoginStatus, message: &str, extra: BTreeMap<String, String>| QRLoginResult {
            source: crate::model::SOURCE_SODA.to_string(),
            key: token.to_string(),
            status,
            message: message.to_string(),
            extra,
            ..Default::default()
        };

    // 二次验证
    if error_code == 2046 {
        let mut flags = extra.clone();
        flags.insert("need_second_verify".to_string(), "true".to_string());
        let result = build(
            QRLoginStatus::Scanned,
            "请在汽水二次验证窗口中完成身份验证",
            flags,
        );
        session.last_result = Some(result.clone());
        session.last_check_ms = now;
        session_save(&session);
        save_state(&session);
        return Ok(result);
    }

    // 过期
    if error_code == 2 || matches!(raw_status.as_str(), "expired" | "expire" | "timeout") {
        let result = build(
            QRLoginStatus::Expired,
            "汽水二维码已过期，请重新生成",
            extra,
        );
        session_remove(token);
        close_browser_session(soda, &session);
        return Ok(result);
    }

    // 7 号：临时限流，绝不判失败
    if error_code == 7 && !confirmed {
        session.cooldown_until_ms = now + RATE_LIMIT_COOLDOWN_MS;
        session.last_check_ms = now;
        let mut flags = extra;
        flags.insert("rate_limited".to_string(), "true".to_string());
        flags.insert("retry_after_ms".to_string(), "60000".to_string());
        let result = build(QRLoginStatus::Waiting, "汽水正在确认登录，请稍候…", flags);
        if session.last_result.is_none() {
            session.last_result = Some(result.clone());
        }
        session_save(&session);
        save_state(&session);
        return Ok(session.last_result.clone().unwrap_or(result));
    }

    if error_code != 0
        && !confirmed
        && !description.contains("访问太频繁")
        && !description.contains("操作频繁")
    {
        return Err(SodaError::not_found(if description.is_empty() {
            format!("汽水扫码失败（{error_code}）")
        } else {
            description
        }));
    }

    if confirmed {
        // 从响应多处收集登录态（等价参考实现的 sessionCookie 列表）
        let mut parts: Vec<String> = Vec::new();
        for key in ["session_cookie", "cookie"] {
            if let Some(value) = data.get(key).and_then(|item| item.as_str()) {
                parts.push(value.to_string());
            }
            if let Some(value) = payload.get(key).and_then(|item| item.as_str()) {
                parts.push(value.to_string());
            }
        }
        for key in ["sessionid", "session_id"] {
            if let Some(value) = data.get(key).and_then(|item| item.as_str()) {
                parts.push(format!("sessionid={value}"));
            }
        }
        if let Some(auth) = data.get("auth") {
            for key in ["sessionid", "session_id"] {
                if let Some(value) = auth.get(key).and_then(|item| item.as_str()) {
                    parts.push(format!("sessionid={value}"));
                }
            }
        }
        session.cookie = merge_cookies(&session.cookie, &parts);

        // 登录态通常既不在响应体里，也不在本次响应的 Set-Cookie 里，而是落在
        // 「签名页浏览器会话」的 cookie jar 中（上游 signer 返回的就是整个上下文
        // cookie）。若本地会话还没拿到 sessionid，就跟着 `redirect_url` 再走一跳，
        // 把会话 cookie 引出来。
        if !has_login_state(&session.cookie) {
            if let Some(redirect) = data
                .get("redirect_url")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                harvest_cookies(soda, &mut session, redirect)?;
            }
        }
        if !has_login_state(&session.cookie) {
            return Err(SodaError::not_found(
                "汽水扫码成功但没拿到登录态：扫码登录依赖签名页承接会话 cookie\
（sodam/libresoda 正在使用内置 Rust CDP 签名页；请检查 Chromium 是否可用），请重新扫码",
            ));
        }
        soda.set_cookie(session.cookie.clone());
        session_remove(token);
        // 登录态已经引到本地，浏览器上下文用完即关，避免设备身份被复用。
        close_browser_session(soda, &session);
        let mut cookies = BTreeMap::new();
        for pair in session.cookie.split(';') {
            if let Some((name, value)) = pair.trim().split_once('=') {
                cookies.insert(name.to_string(), value.to_string());
            }
        }
        let result = QRLoginResult {
            source: crate::model::SOURCE_SODA.to_string(),
            key: token.to_string(),
            status: QRLoginStatus::Success,
            message: "登录成功".to_string(),
            cookie: session.cookie.clone(),
            cookies,
            extra,
        };
        return Ok(result);
    }

    // 默认：等待（含扫码已发生但未确认）
    let status = if raw_status == "scanned" || raw_status == "2" {
        QRLoginStatus::Scanned
    } else {
        QRLoginStatus::Waiting
    };
    let message = if status == QRLoginStatus::Scanned {
        "已扫码，请在手机上确认"
    } else {
        "等待扫码"
    };
    let result = build(status, message, extra);
    session.last_result = Some(result.clone());
    session.last_check_ms = now;
    session_save(&session);
    save_state(&session);
    Ok(result)
}

impl Soda {
    /// 创建扫码登录二维码。
    pub fn create_qr(&self) -> Result<QrCreateResult> {
        create_qr(self)
    }

    /// 轮询扫码状态。
    pub fn check_qr(&self, token: &str) -> Result<QRLoginResult> {
        check_qr(self, token)
    }
}

#[cfg(test)]
mod rng_tests {
    use super::*;

    /// 回归测试：RNG 播种必须是**有界**的。
    ///
    /// 历史事故：`Rng::new()` 曾用 `std::fs::read("/dev/urandom")`，在 Linux 上会一直读到
    /// 内存耗尽（实测 1 秒吃满 512MB 后被 OOM kill，整机都拖死）。这个用例在旧实现下会
    /// 直接卡死/被 OOM 杀掉，因此能守住这类回归。
    #[test]
    fn rng_seeding_is_bounded_and_usable() {
        let mut rng = Rng::new();
        let digits = rng.digits(16);
        assert_eq!(digits.len(), 16, "设备号应为 16 位");
        assert!(digits.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(digits.chars().next().unwrap(), '0', "首位应为 1-8");

        let token = random_ms_token();
        assert!(!token.is_empty(), "msToken 不该为空");
        // 形态必须与上游参考实现一致：88 字节 → base64url（118 字符）+"=="，共 120
        assert_eq!(token.len(), 120, "msToken 长度应为 120: {}", token.len());
        assert!(token.ends_with("=="), "msToken 应以 == 结尾");
        assert!(
            token[..118]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "msToken 主体必须是 base64url 字符：{token}"
        );
    }

    #[test]
    fn seed_has_entropy_and_is_not_all_zero() {
        let a = random_seed();
        let b = random_seed();
        assert!(a.iter().any(|byte| *byte != 0), "种子不该全 0");
        assert_ne!(a, b, "两次取种子不应该一样");
    }
}
