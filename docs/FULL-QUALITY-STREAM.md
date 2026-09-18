# 整曲 / 高音质取流（x-helios + x-medusa）

> 本文回答一个具体问题：**为什么 SVIP 账号在 libresoda 里只拿到 30 秒试听，
> 以及怎么才能拿到整曲（含无损）。**
>
> 结论先行：**不是账号权限问题**，而是"整曲流的接口要求应用级请求签名"。
> 签名由官方客户端的原生安全组件生成，纯软件侧算不出来，必须从官方客户端
> 的真实请求里取。

---

## 1. 三条取流路径的实测结论

汽水的播放流分三层，能力完全不同（下表全部为本机实测，曲目
`7501674235158431760`「下完这场雨」，账号 `is_vip=true`）：

| 层 | 端点 | 是否需要签名 | 实测返回 |
| --- | --- | --- | --- |
| 分享页 / SEO | `GET beta-luna.douyin.com/luna/h5/seo_track` | 否 | 曲目元数据 + `bit_rates`（含整曲体积）+ **30s/60s 试听流** |
| Web / H5 | `GET /luna/h5/track_v2`、`GET /luna/pc/track_v2?device_platform=web&channel=pc_web` | 否（带 `a_bogus` 也一样） | 同样是试听流：`video_model.video_duration=30.001` |
| **App** | `POST api.qishui.com/luna/pc/track_v2` | **是，且逐请求**（`x-helios` + `x-medusa`） | 未签名 → `HTTP 200` + **0 字节**；签名与"URL + body 字节"完全匹配 → 整曲播放流 |

关键证据：

1. 同一曲目、同一个已登录 Cookie（`/luna/h5/me` 返回 `status_code=0`，说明登录态有效），
   SEO 与 H5 端点都只给试听片段：

   ```text
   track.duration = 236620ms        track.preview.duration = 30001ms
   bit_rates = [medium 1956083B, higher 3910131B, highest 7697091B]   ← 整曲体积
   video_model.video_duration = 30.001                                ← 实际下发的流
   ```

2. App 端点在没有应用签名头时**不是报错而是返回空 body**：

   ```text
   POST /luna/pc/track_v2   → HTTP 200, 0 bytes
   ```

   上游 music-lib 的注释把这种现象记成了「PC track_v2 已下线」，实际上接口还活着，
   只是风控把响应体挖空了。

3. 用 Web 侧签名（本仓库扫码登录那条链路，`bdms` 生成、`a_bogus` 长度 44，已验证）
   去请求 App 端点，**仍然是空 body** —— 说明 App 端点只认应用级签名头。

4. 官方客户端抓包（见第 4 节）拿到的真实请求：

   ```http
   POST /luna/pc/track_v2?aid=386088&app_name=luna_pc&...&device_id=<device_id>&...&fp=<device_id>
   X-Helios: <48-byte captured value>
   X-Medusa: <872 字符>
   X-SS-STUB: <body 的 MD5 大写>
   user-agent: LunaPC/3.8.0(467160162)
   x-luna-is-local-user: 0
   ```

   **`x-helios` / `x-medusa` 是逐请求签名**：同一次抓包里 13 个请求对应 13 组不同的值。

5. 用抓到的签名做重放实验（本机 Linux 直连），结论非常干脆：

   | 变体 | 结果 |
   | --- | --- |
   | 原样重放（同 URL、同 body、同签名） | `HTTP 200`，**29455 B**，`track="Fever Pitch"`、`video_duration=135.484`、6 条流含 `lossless`(flac) / `hi_res` |
   | 2 分钟后再放一次（签名寿命 ≥107s） | 同样 `HTTP 200`，29383 B |
   | 同一签名，**只改 JSON 键顺序** | `HTTP 200`，**0 字节** |
   | 同一签名，换 `track_id` | `HTTP 200`，**0 字节** |
   | 同一签名，换 `queue_type`/`scene_name` | `HTTP 200`，**0 字节** |
   | 同一签名，改用网页登录 Cookie | 仍能拿到整曲 → 签名不绑定 Cookie，只绑定请求本身 |

   即：**签名 = 对这次请求（URL + body 字节）的签名，能重放、不能改。**
   这就是为什么"抓一次凭证长期用"只在**同一请求原样重放**时成立，
   而 Meting-API 要把签名做成一个**远程服务**（`QISHUI_SIGNER_URL`）而不是静态值。

---

## 2. 为什么纯软件算不出来

对三份材料交叉验证后的结论：

| 材料 | 做法 | 对我们的意义 |
| --- | --- | --- |
| `guohuiyuan/music-lib`（本 crate 的上游） | SEO 拿明文试听流；VIP 曲目再走 `POST /luna/pc/track_v2`（`LunaPC/3.3.0(359450208)` + `x-luna-*`） | 逻辑对，但缺应用签名头 → 空 body，所以上游把它标成"已下线" |
| `qq01-hub/Meting-API` | 登录用 headless Chromium 跑官方安全组件（`bdms`）拿 `a_bogus`；取流则要求外挂一个 `QISHUI_SIGNER_URL` 远程签名服务返回 `X-Helios`/`X-Medusa` | 与我们的实测完全一致：**Web 签名够不着 App 端点**，取流必须另有签名来源 |
| `520Qiuyu/qishuiMusicAnalysis`（92★） | `.env` 里填 `DEVICE_ID` + `COOKIE` + `X_HELIOS` + `X_MEDUSA`，从**汽水音乐电脑版抓包**得到 | 社区标准做法：抓包回填 |
| `guowenye/qishui-api` | 同样把 `QISHUI_X_HELIOS` / `QISHUI_X_MEDUSA` 做成环境变量 | 同上 |
| 汽水 sourcemap 泄漏源码（193 个 TS 文件） | `src/services/request/request.ts` 只负责拼公共参数；`src/libs/bdticket/config.ts` 的 `session_guard` 名单里**没有** `track_v2`（说明这不是 session guard，而是原生层签名） | 签名不在 JS 里，无法移植 |
| 客户端安装目录 | `mssdk/metasecml.dll`（6.8MB，导出 `MSBridgeML` / `MSBridgeOV` / `o0o0`）| `x-helios` / `x-medusa` 由它生成；在 `main.asar` / `app.asar` / `metasecml.dll` 里**搜不到任何 `helios` / `medusa` 字符串**（混淆/加密），目前没有公开的纯软件复刻 |

因此本 crate 的策略是：**把"签名来源"做成可插拔的凭证**，而不是假装能算出来。

---

## 3. libresoda 的接法

```rust
use libresoda::Soda;

let soda = Soda::new(cookie);                       // 扫码登录 / 手动 Cookie
soda.load_app_credentials("/etc/libresoda/qishui-credentials.json")?;

// 一眼看清"拿到的是整曲还是试听、缺什么"
let report = soda.check_stream_access("7501674235158431760")?;
println!("整曲={} 试听={} 音质={} 提示={}",
    report.is_full_track(), report.is_preview,
    report.best.as_ref().map(|info| info.quality.as_str()).unwrap_or("-"),
    report.hint);

// 正常下载路径会自动优先整曲
let song = soda.parse("https://qishui.douyin.com/s/xxxxxx/")?;
let info = soda.get_download_info(&song)?;
assert!(!info.is_preview, "试听片段会被标记出来：{}", info.note);
```

凭证文件格式（JSON，字段可省略；`fp` 缺省等于 `device_id`）：

```json
{
  "device_id": "<device_id>",
  "iid": "<install_id>",
  "fp": "<device_id>",
  "x_helios": "……",
  "x_medusa": "……",
  "user_agent": "LunaPC/3.8.0(467160162)"
}
```

`DownloadInfo` 新增两个字段，下游客户端应当据此显示"试听"角标：

| 字段 | 含义 |
| --- | --- |
| `is_preview` | 该流明显短于整曲（30s/60s 试听） |
| `note` | 人话说明，例如"试听片段：整曲需要应用签名凭证（x-helios / x-medusa）" |

### 3.1 逐请求签名：接实时签名器（推荐）

既然签名与请求绑死，正确的接法是**每次取流都找签名器要一对新的
`x-helios` / `x-medusa`**。libresoda 把这一步接在 `POST /luna/pc/track_v2` 上：
只要设置了 [`signature::SignatureProvider`](../src/soda/signature.rs)，取流请求就会
先把 `{url, method, body}` 交给它，再用返回的 `headers` 补 `x-helios` / `x-medusa`
（`ms_token` / `a_bogus` 非空时会追加到查询参数）。

```rust
use std::sync::Arc;
use libresoda::{AppCredentials, Soda};
use libresoda::soda::signature::CommandSignature;

let soda = Soda::new(cookie);
// 设备指纹（device_id / iid / fp）来自同一次抓包
soda.set_app_credentials(AppCredentials {
    device_id: "<device_id>".into(),
    iid: "<install_id>".into(),
    fp: "<device_id>".into(),
    ..Default::default()
});
// 签名器：stdin 收 {"url","method","body","ts_ms"}，stdout 回 {"headers":{"x-helios":..,"x-medusa":..}}
soda.set_signature_provider(Arc::new(CommandSignature::new("ssh").args([
    "win-box", "C:\\tools\\qishui-signer.exe",
])));
```

有了签名器之后，`check_stream_access()` 会直接给出整曲；没有它时，
它会明确告诉你缺什么，而不是静默返回 30 秒试听。

---

## 4. 抓包步骤（Linux 抓包机 + Windows 客户端）

本仓库自带 [`tools/qishui-capture/api_proxy.py`](../tools/qishui-capture/api_proxy.py)：
终止 TLS、记录请求（Cookie 只记字段名，完整 Cookie 与签名写进 0600 的 `--secret-log`），
再把请求转发给真实上游，客户端可以继续正常使用。

```bash
# ① 抓包机上生成证书（CN/SAN 必须是 api.qishui.com）
mkdir -p /tmp/qishui-mitm && cd /tmp/qishui-mitm
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 30 -subj "/CN=Qishui Capture CA"
openssl req -newkey rsa:2048 -nodes -keyout api.key -out api.csr -subj "/CN=api.qishui.com"
printf 'subjectAltName=DNS:api.qishui.com\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' > ext.cnf
openssl x509 -req -in api.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out api.crt -days 30 -extfile ext.cnf

# ② 启动代理（443 需要 root）
sudo python3 tools/qishui-capture/api_proxy.py \
  --port 443 --cert /tmp/qishui-mitm/api.crt --key /tmp/qishui-mitm/api.key \
  --log /tmp/qishui-mitm/capture.log --secret-log /tmp/qishui-mitm/secrets.log

# ③ 客户端机器：把 api.qishui.com 指向抓包机，并把 ca.crt 导入"受信任的根证书颁发机构"
#    Windows: C:\Windows\System32\drivers\etc\hosts 加一行，然后 ipconfig /flushdns

# ④ 重启客户端 + 随便播一首歌，然后从 secrets.log 里取 device_id / cookie / x_helios / x_medusa
```

不想自己搭代理的话，用 [Reqable](https://reqable.com/zh-CN/)（小黄鸟）等 HTTPS 抓包工具
按同样字段抄下来即可 —— 社区项目都是这条路。

**用完记得回滚**：删掉 hosts 里那一行、移除导入的根证书。

---

## 5. 已知限制 / 待办

* **签名与请求绑定**（已实测，见第 1 节第 5 条）。所以：
  * 静态凭证只能"原样重放抓到的那个请求"，无法用来点播任意歌曲；
  * 想要任意歌曲的整曲，必须有**实时签名器**：官方客户端本体，或调
    `mssdk/metasecml.dll` 的桥接器（`docs/MSSDK-PORTING.md` 里的路线）。
* 签名有寿命（实测 ≥107 秒可重放，更久未测），所以签名器必须在线。
* 目前没有公开的"纯软件复刻 mssdk"实现：`main.asar` / `app.asar` / `metasecml.dll`
  里都搜不到 `helios` / `medusa` 明文，社区（music-lib / Meting-API / qishui-api /
  qishuiMusicAnalysis）无一例外都是"抓官方客户端 / 外挂签名服务"。
* 合规提醒：这些凭证代表你自己的账号与设备，只应在本机自用；不要分发、不要做成公共代理。

### 已解决：签名服务（分离架构，实测通过）

逆向工作的结论出乎意料地好：**不用啃 `metasecml.dll`**。官方客户端把签名调用
放在 `resources/app.asar.unpacked/bdms.node` 里，导出面很干净：

```text
bdms.node → init({deviceId}) / generateHttpSignatureHeaders(url, headerLines) / report
```

sourcemap 泄漏的 `src/app.ts` 里有官方用法（把请求头展平成 `k\r\nv` 交给它，
返回值按 name/value 成对写回 headers）。据此实现的桥接器
[`mssdk-bridge.mjs`](../tools/qishui-signer-host/mssdk-bridge.mjs) 已经跑通：

| 条件 | 结果 |
| --- | --- |
| VIP-only 曲目 + 无签名 | `HTTP 200`，`video_duration=30`，单档 `higher`（试听） |
| 同一曲目 + `bdms.node` 现场签名 + SVIP Cookie | `HTTP 200`，26529 B，**`video_duration=236.62`**（整曲），**6 档全出**：medium / higher / spatial / **highest** / hi_res / **lossless(flac)** |

部署与契约见 [`docs/SIGNER-SERVICE.md`](SIGNER-SERVICE.md)：
Windows 跑 `signer-service.mjs`（`backend=command` → `mssdk-bridge.mjs`），
Linux 侧 `HttpSignature::new("http://win-box:8899/sign")` 或 `QISHUI_SIGNER_URL`。

验收：`stream_access_diagnose_online`（见 `tests/network_tests.rs`），
配置 `SODA_DEVICE_ID` + `QISHUI_SIGNER_URL` 后期望 `!is_preview`。
