# 汽水签名服务（Windows 侧）

> 这里是一份**精简内置版**，便于在 libresoda 仓库里就近联调；
> 产品化版本（含 `/probe` 真机自检、Bearer 鉴权、CLI、测试与 CI）在
> **<https://github.com/sodahub-org/libmssdk>**。

把「生成 `x-helios` / `x-medusa`」这件事抽成一个独立服务，跑在唯一能跑原生组件的
机器上（Windows；macOS 版客户端同理），Linux 侧的 libresoda 通过 HTTP 调用。
这样 **第三方开源客户端不需要任何 Windows 依赖**，只要配一个 `QISHUI_SIGNER_URL`。

```text
[libresoda / 第三方客户端 (Linux)]
        │ POST /sign  {"url","method","body","ts_ms"}
        │ ← {"ok":true,"headers":{"x-helios":"…","x-medusa":"…"}}
        ▼
[本服务 signer-service.mjs (Windows)] ──► [mssdk / 官方客户端]
```

## 1. 为什么必须分离

实测结论（见 `docs/FULL-QUALITY-STREAM.md`）：

| 事实 | 后果 |
| --- | --- |
| 整曲端点 `POST /luna/pc/track_v2` 要求 `x-helios` / `x-medusa` | 没有这两个头就只有 30～60 秒试听 |
| 签名与 **URL + body 字节**绑定（改一个字节就作废） | 不能"抓一次存起来长期用" |
| 签名由 `mssdk/metasecml.dll` 生成，只有 Windows/macOS 版 | 纯 Linux 进程算不出来 |
| 公开项目（music-lib / Meting-API / qishui-api / qishuiMusicAnalysis） | 无一例外：抓官方客户端 / 外挂签名服务 |

所以合理解只有一条：**签名留在 Windows，取流留在 Linux**。

## 2. 部署（Windows）

```powershell
# 需要 Node.js 18+（官方客户端自带 Electron，但没有独立 node.exe，装一个即可）
node --version

# 联调：先用 echo 后端确认链路通
$env:QISHUI_SIGNER_BACKEND='echo'; node signer-service.mjs
# 自检
node signer-service.mjs --selftest

# 生产：接真实桥接器
$env:QISHUI_SIGNER_BACKEND='command'
$env:QISHUI_SIGNER_COMMAND='C:\tools\qishui-bridge\mssdk-bridge.exe'
$env:QISHUI_SIGNER_TOKEN='换成你的随机串'
$env:BIND='192.168.1.5'
$env:PORT='8899'
node signer-service.mjs
```

常驻建议：`nssm install QishuiSigner`，或
`schtasks /create /tn QishuiSigner /sc onstart /ru SYSTEM /tr "node C:\tools\qishui-signer-host\signer-service.mjs"`。
防火墙只放行内网网段；跨公网一律走 SSH 隧道，不要把 8899 暴露到互联网。

## 3. 后端怎么选

### A. `command`：桥接器调官方客户端的 `bdms.node`（**已跑通，推荐**）

**不需要逆向 `metasecml.dll`**。官方客户端自己就是把签名交给
`resources/app.asar.unpacked/bdms.node` 的，导出面很干净：

```text
bdms.node        → init, generateHttpSignatureHeaders, report
bdticket.node    → startBDTicket, handleRequest, handleResponse, refreshSettings, registerEventEmitter
device.node      → applogDecorated, getSerial, getComputerName, getChannelId, decodeSpade
```

sourcemap 泄漏的 `src/app.ts` 里能直接读到官方调用方式：

```ts
bdms.init({ deviceId: deviceData.did ?? fakeDid })
// 每个发往 qishui.com 的请求（/passport/、/ttwid/ 除外）：
headers['user-agent'] = [`LunaPC/${APP_VERSION}(${TRON_BUILD_ID})`]
const headerLines = /* 把 headers 展平成 "名字\r\n值\r\n…" */
const signatureData = bdms
  .generateHttpSignatureHeaders(params.url, headerLines.join('\r\n'))
  .split('\r\n').filter(t => t.trim())
for (let i = 0; i < signatureData.length / 2; i++)
  headers[signatureData[i * 2]] = [signatureData[i * 2 + 1]]
```

本仓库已经把它写成可直接用的桥接器
[`mssdk-bridge.mjs`](mssdk-bridge.mjs)：stdin 进 `{url,method,body,headers,ts_ms}`，
stdout 出 `{"headers":{"X-Helios":"…","X-Medusa":"…"}}`。

```powershell
$env:QISHUI_SIGNER_BACKEND='command'
$env:QISHUI_SIGNER_COMMAND='node C:\qishui-signer\mssdk-bridge.mjs'
$env:QISHUI_CLIENT_DIR='C:\Users\<你>\AppData\Local\Programs\Soda Music\3.7.0'
node signer-service.mjs
```

**实测结果**（VIP-only 曲目 `7501674235158431760`《下完这场雨》，SVIP Cookie）：

```text
SIG_RAW_LEN 900  pairs 2        ← X-Helios(48) + X-Medusa(828)
HTTP 200 bytes 26529
TRACK 下完这场雨 duration 236620 video_duration 236.62
GEARS [medium, higher, spatial+boost, highest, hi_res, lossless(flac)]
```

对比：没有签名时同一首歌只回 30 秒试听（`video_duration=30`，单档 `higher`）。

两个必须对齐的点：

1. **签名覆盖 URL + 请求头**，所以桥接器拿到的 `headers` 必须与真实发送的一致
   （尤其 `cookie`、`x-ss-stub` = body 的 MD5 大写）；
2. `device_id` 要用签名器里 `init()` 的那台设备的 did（抓包得到），URL 里的
   `device_id` / `fp` 要与之相同。

> 备选：真要"不依赖客户端安装目录"，才需要啃 `metasecml.dll`。目前
> `main.asar` / `app.asar` / `metasecml.dll` 里都搜不到 `helios` / `medusa` 明文
> （字符串混淆/加密），没有公开复刻，性价比远低于直接用 `bdms.node`。

### B. `command`：借官方客户端自己的请求管道（零逆向，最省事）

不碰 DLL，直接把官方客户端当"签名 oracle"：

1. 客户端以调试模式启动：`SodaMusic.exe --remote-debugging-port=9333 --inspect=9229`
   （`--inspect` 打开主进程的 Node inspector —— 请求是主进程里发的）。
2. 用 `chrome-remote-interface` / `ws` 连上 `9229`，在 `Runtime.evaluate` 里调用
   客户端自己的请求服务（`protocolOf(ServiceId.request)` 那一层），把
   `GetTrackV2` 的结果回吐给本服务。
3. 于是 `x-helios` / `x-medusa` 由客户端自己算，我们只转发。

代价：客户端必须常驻；好处：不用逆向，接口变动也能跟着走。

### C. `http`：多级中转

把 `QISHUI_SIGNER_UPSTREAM` 指到另一个签名服务（例如把 A/B 跑在别的机器上，
本机只做鉴权与限流）。

### D. `echo`：只做联调

返回假签名，用来确认「客户端 → 服务 → libresoda」链路与鉴权是否正常；
用它取不到整曲流（服务端会照旧回空 body）。

## 4. 自检与排错

```bash
curl http://win-box:8899/healthz
# {"ok":true,"backend":"command","auth":true}

curl -X POST http://win-box:8899/sign \
  -H 'Authorization: Bearer <token>' -H 'Content-Type: application/json' \
  -d '{"url":"https://api.qishui.com/luna/pc/track_v2?aid=386088","method":"POST","body":"{\"track_id\":\"7501674235158431760\",\"media_type\":\"track\"}"}'
# {"ok":true,"headers":{"x-helios":"…","x-medusa":"…"}}
```

Linux 侧验收（libresoda）：

```bash
QISHUI_SIGNER_URL=http://win-box:8899/sign \
SODA_COOKIE="sessionid_ss=..." \
  cargo test --offline -- --ignored --nocapture stream_access_diagnose_online
```

日志只记录「请求形状 + 签名长度」，不会把凭据写进文件；服务默认要求 Bearer token。
这些签名等价于你的账号+设备凭据，**只在本机/内网自用，不要公开部署或分发**。
