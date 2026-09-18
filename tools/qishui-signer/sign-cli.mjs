// 给 libresoda 的 CommandRequester 用的一次性客户端：
//   stdin : {"sessionKey":"...","method":"POST","url":"...","headers":{...},"body":"...","msToken":"..."}
//           {"op":"close","sessionKey":"..."}   ← 关闭该会话的浏览器上下文
//   stdout: {"ok":true,"status":200,"body":"...","responseURL":"...","cookies":[...]}
// 需要先在本地启动 signer-server.mjs（可用 nohup 常驻）。

const RAW_ENDPOINT = process.env.QISHUI_SIGNER_URL || 'http://127.0.0.1:8799/request'
const ENDPOINT = /\/request$/.test(RAW_ENDPOINT)
  ? RAW_ENDPOINT
  : `${RAW_ENDPOINT.replace(/\/$/, '')}/request`
const CLOSE_ENDPOINT = ENDPOINT.replace(/\/request$/, '/close')

const chunks = []
for await (const chunk of process.stdin) chunks.push(chunk)
const raw = Buffer.concat(chunks).toString('utf8') || '{}'

try {
  const payload = JSON.parse(raw)
  const closing = String(payload.op || '').toLowerCase() === 'close'
  const body = closing ? JSON.stringify({ sessionKey: payload.sessionKey }) : raw
  const response = await fetch(closing ? CLOSE_ENDPOINT : ENDPOINT, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body,
  })
  const text = await response.text()
  if (!response.ok) {
    process.stdout.write(JSON.stringify({ ok: false, error: `signer HTTP ${response.status}: ${text.slice(0, 200)}` }))
    process.exit(0)
  }
  // 关闭会话时上层只关心成功与否，回一个最小响应即可
  process.stdout.write(closing ? JSON.stringify({ ok: true, closed: true }) : text)
} catch (error) {
  process.stdout.write(JSON.stringify({ ok: false, error: `signer 不可用: ${error.message}` }))
}
