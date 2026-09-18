// 汽水签名服务：复刻 Meting-API 的 providers/qishui/signer.js
//
// 玩法：把官方安全组件（security_host.html + sdk-glue.js + bdms.js）跑在真实
// Chromium 里，由页面内的 window.__qishuiRequest 完成「带签名」的请求
// （URL 会被补上 a_bogus 等参数，并带上 X-Helios / X-Medusa 头）。
//
// 接口：
//   GET  /health                                  → "ok"
//   POST /request   {sessionKey,method,url,headers,body,msToken}
//        → {ok,status,body,responseURL,headers,cookies}
//   POST /close     {sessionKey}                   → 关闭该会话的浏览器上下文
//   GET  /sessions                                 → 当前会话列表（排查用）
//
// 会话隔离（对齐 Meting-API signer.js 的 `pages`）：
//   每个 sessionKey 一个独立 BrowserContext（独立 cookie jar + 独立设备身份）。
//   不隔离的话，同一个设备身份连着开多个二维码会话会被护照服务限流
//   （error_code=7），这正是本项目早期一直「扫不上」的原因之一。
//
// 启动： node signer-server.mjs            （默认端口 8799）
// 环境： CHROMIUM_PATH 指定 Chromium 可执行文件，PORT 指定端口。

import http from 'node:http'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const __dirname = path.dirname(fileURLToPath(import.meta.url))
const SECURITY_DIR = path.join(__dirname, 'security')
const PORT = Number(process.env.PORT || 8799)
const CHROMIUM = process.env.CHROMIUM_PATH || '/usr/local/bin/chromium'
const UA = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) SodaMusic/3.2.1 Chrome/136.0.7103.59 Electron/36.4.0 Safari/537.36'

function loadPlaywright() {
  const candidates = [
    process.env.PLAYWRIGHT_CORE,
    'playwright-core',
    'playwright',
  ].filter(Boolean)
  for (const candidate of candidates) {
    try {
      return require(candidate)
    } catch {
      /* 继续尝试下一个 */
    }
  }
  throw new Error('找不到 playwright-core；请设置 PLAYWRIGHT_CORE 指向其 index.js')
}

const { chromium } = loadPlaywright()

let browserPromise = null

// 按会话隔离的浏览器上下文（对齐 Meting-API 的 signer.js）：
//   key = sessionKey → { context, page, lastUsed }
//
// 为什么必须隔离：护照服务会按「设备身份」限流，而 bdms 的设备身份是在页面加载时
// 生成、并挂在页面/localsStorage/Cookie 上的。共用一个页面连着开很多二维码会话，
// 很快就会被判「访问太频繁」（error_code=7）；每个二维码一个新上下文就没这个问题。
const sessions = new Map()
const SESSION_IDLE_MS = 5 * 60 * 1000
const DEFAULT_SESSION_KEY = 'default'

async function getBrowser() {
  if (!browserPromise) {
    browserPromise = chromium.launch({
      executablePath: CHROMIUM,
      args: [
        '--no-sandbox',
        '--disable-setuid-sandbox',
        '--disable-dev-shm-usage',
        '--disable-gpu',
        '--disable-extensions',
        '--disable-background-networking',
        '--disable-sync',
        '--disable-default-apps',
        '--disable-component-update',
        '--renderer-process-limit=1',
        // Passport 在本地安全页发起跨站 XHR；必须放开跨域与第三方 Cookie，
        // 否则会出现「扫码状态成功但 sessionid 写不进隔离会话」。
        '--disable-web-security',
        '--disable-features=IsolateOrigins,site-per-process,BlockThirdPartyCookies,ThirdPartyStoragePartitioning',
        '--disable-third-party-cookies',
      ],
    })
  }
  return browserPromise
}

async function createSession(sessionKey) {
  const entry = {
    context: null,
    page: null,
    promise: null,
    lastUsed: Date.now(),
  }
  entry.promise = (async () => {
    const browser = await getBrowser()
    const context = await browser.newContext({ userAgent: UA })
    const page = await context.newPage()
    const diagnostics = []
    page.on('pageerror', error => diagnostics.push(`pageerror:${error.message}`))
    page.on('console', message => diagnostics.push(`console:${message.type()}:${message.text()}`))

    // 页面会去 CDN 取 bdms.js —— 这里用本地文件应答（等价参考实现的请求拦截）
    await page.route(/bdms\.js(\?|$)/, route =>
      route.fulfill({
        status: 200,
        contentType: 'text/javascript; charset=utf-8',
        body: fs.readFileSync(path.join(SECURITY_DIR, 'bdms.js')),
      }),
    )
    await page.goto(`http://127.0.0.1:${PORT}/security_host.html`, { waitUntil: 'load', timeout: 30000 })
    try {
      await page.waitForFunction(() => Boolean(window.bdms), null, { timeout: 30000 })
    } catch (error) {
      const state = await page
        .evaluate(() => ({
          bdms: typeof window.bdms,
          glue: typeof window._sdkGlueVersionMap,
          traces: (window.__qishuiSecurityTrace || []).slice(-10),
        }))
        .catch(() => ({}))
      throw new Error(`安全组件初始化失败: ${error.message} ${JSON.stringify({ diagnostics: diagnostics.slice(-10), state })}`)
    }
    entry.context = context
    entry.page = page
    return entry
  })().catch(async error => {
    sessions.delete(sessionKey)
    if (entry.context) await entry.context.close().catch(() => {})
    throw error
  })
  sessions.set(sessionKey, entry)
  return entry.promise
}

async function getSession(sessionKey = DEFAULT_SESSION_KEY) {
  const key = String(sessionKey || DEFAULT_SESSION_KEY)
  const existing = sessions.get(key)
  if (existing) {
    existing.lastUsed = Date.now()
    return existing.promise
  }
  return createSession(key)
}

async function closeSession(sessionKey) {
  const key = String(sessionKey || DEFAULT_SESSION_KEY)
  const entry = sessions.get(key)
  sessions.delete(key)
  if (!entry) return false
  try {
    const { context } = await entry.promise
    if (context) await context.close()
  } catch {
    /* 关闭失败无所谓：上下文随后会随浏览器一起退出 */
  }
  return true
}

/// 空闲会话回收：避免用户反复扫码后累积一堆 Chromium 上下文。
async function cleanupIdleSessions() {
  const now = Date.now()
  for (const [key, entry] of [...sessions]) {
    if (!entry.page) continue // 还在初始化，别动它
    if (now - entry.lastUsed < SESSION_IDLE_MS) continue
    await closeSession(key)
  }
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = []
    request.on('data', chunk => chunks.push(chunk))
    request.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')))
    request.on('error', reject)
  })
}

function serveAsset(request, response, pathname) {
  const name = path.basename(pathname) || 'security_host.html'
  const file = path.join(SECURITY_DIR, name)
  if (!file.startsWith(SECURITY_DIR) || !fs.existsSync(file)) {
    response.statusCode = 404
    response.end('not found')
    return
  }
  response.setHeader('content-type', name.endsWith('.html') ? 'text/html; charset=utf-8' : 'text/javascript; charset=utf-8')
  fs.createReadStream(file).pipe(response)
}

const server = http.createServer(async (request, response) => {
  const url = new URL(request.url || '/', `http://127.0.0.1:${PORT}`)
  try {
    if (url.pathname === '/health') {
      response.end('ok')
      return
    }
    if (url.pathname === '/request' && request.method === 'POST') {
      const payload = JSON.parse(await readBody(request))
      const { page, context } = await getSession(payload.sessionKey)
      // 允许调用方注入会话 Cookie（用于把已登录的会话带进签名上下文）
      if (Array.isArray(payload.cookies) && payload.cookies.length > 0) {
        const cookies = payload.cookies
          .filter(item => item && item.name && item.value)
          .map(item => ({
            name: String(item.name),
            value: String(item.value),
            domain: String(item.domain || '.qishui.com'),
            path: String(item.path || '/'),
          }))
        if (cookies.length > 0) await context.addCookies(cookies)
      }
      if (payload.msToken) {
        await page.evaluate(token => {
          localStorage.setItem('xmst', token)
          localStorage.setItem('xmsty', token)
        }, payload.msToken)
      }
      // Cookie 由浏览器自己管理：XHR 的 withCredentials 会带上本会话 jar 里的 cookie。
      // 显式传 Cookie 头反而会被浏览器丢弃（forbidden header），这里直接剔除，
      // 避免调用方误以为它生效（对齐 Meting-API 的 safeHeaders 处理）。
      const headers = { ...(payload.headers || {}) }
      for (const name of Object.keys(headers)) {
        if (/^cookie$/i.test(name)) delete headers[name]
      }
      const result = await page.evaluate(
        async spec => window.__qishuiRequest(spec),
        {
          method: String(payload.method || 'GET').toUpperCase(),
          url: String(payload.url || ''),
          headers,
          body: payload.body ?? null,
          timeout: 180000,
        },
      )
      // 签名缺失在这里就报错，而不是让上层拿到一个「没签名的 200」
      // —— 那样会一路撞 error_code=7，还以为是限流（实测踩过）。
      const signed = new URL(String(result?.responseURL || payload.url || 'https://x/'))
      const aBogus = signed.searchParams.get('a_bogus') || ''
      if (aBogus.length !== 44) {
        response.statusCode = 502
        response.setHeader('content-type', 'application/json')
        response.end(
          JSON.stringify({
            ok: false,
            error: `汽水安全签名生成失败：a_bogus 缺失（${aBogus.length} 字符）`,
          }),
        )
        return
      }
      const cookies = await context.cookies()
      response.setHeader('content-type', 'application/json')
      response.end(JSON.stringify({ ok: true, ...result, cookies }))
      return
    }
    if (url.pathname === '/close' && request.method === 'POST') {
      const payload = JSON.parse(await readBody(request))
      const closed = await closeSession(payload.sessionKey)
      response.setHeader('content-type', 'application/json')
      response.end(JSON.stringify({ ok: true, closed }))
      return
    }
    if (url.pathname === '/sessions') {
      response.setHeader('content-type', 'application/json')
      response.end(
        JSON.stringify({
          ok: true,
          sessions: [...sessions].map(([key, entry]) => ({
            key,
            ready: Boolean(entry.page),
            idleMs: Date.now() - entry.lastUsed,
          })),
        }),
      )
      return
    }
    serveAsset(request, response, url.pathname)
  } catch (error) {
    response.statusCode = 500
    response.setHeader('content-type', 'application/json')
    response.end(JSON.stringify({ ok: false, error: String(error && error.message ? error.message : error) }))
  }
})

server.listen(PORT, '127.0.0.1', () => {
  console.log(`qishui signer listening on http://127.0.0.1:${PORT} (chromium: ${CHROMIUM})`)
})

const cleanupTimer = setInterval(() => {
  cleanupIdleSessions().catch(() => {})
}, 60_000)
cleanupTimer.unref?.()

const shutdown = async () => {
  try {
    if (browserPromise) {
      const browser = await browserPromise
      await browser.close()
    }
  } finally {
    process.exit(0)
  }
}
process.on('SIGINT', shutdown)
process.on('SIGTERM', shutdown)
