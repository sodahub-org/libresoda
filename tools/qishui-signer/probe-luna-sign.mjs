// 实验脚本：验证「官方 Web 安全组件（bdms）能否给 /luna/* 内容接口加签」。
//
// 背景：api.qishui.com 的 /luna/pc/me 裸调能拿到数据，而 /luna/pc/track_v2、
// /luna/pc/search/track、/luna/media-player 裸调一律返回 200 + 空 body。
// 官方登录页把 bdms 的加签范围配置成 paths: ['/passport']，因此本脚本把这
// 个范围改成待验证的路径，再用同一个 XHR 通道发真实请求，观察：
//   1) responseURL 是否被追加了 a_bogus（长度 44）
//   2) 响应体是否从空变为真实数据（特别是是否返回完整时长的播放流）
//
// 用法：
//   node probe-luna-sign.mjs --track 7501674235158431760 --paths /luna,/passport
//   node probe-luna-sign.mjs --track 7304719759323564095 --paths /luna --no-mstoken
//
// 依赖：本机 Chromium + playwright-core（与 signer-server.mjs 相同）。

import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const __dirname = path.dirname(fileURLToPath(import.meta.url))
const SECURITY_DIR = path.join(__dirname, 'security')
const CHROMIUM = process.env.CHROMIUM_PATH || '/usr/local/bin/chromium'

function loadPlaywright() {
  const candidates = [
    process.env.PLAYWRIGHT_CORE,
    'playwright-core',
  ]
  for (const candidate of candidates) {
    try {
      return require(candidate)
    } catch {
      /* 继续尝试 */
    }
  }
  throw new Error('找不到 playwright-core；请设置 PLAYWRIGHT_CORE')
}

const argv = process.argv.slice(2)
const argValue = (name, fallback = '') => {
  const index = argv.indexOf(`--${name}`)
  return index >= 0 && argv[index + 1] ? argv[index + 1] : fallback
}
const hasFlag = name => argv.includes(`--${name}`)

const trackId = argValue('track', '7501674235158431760')
const paths = argValue('paths', '/luna').split(',').map(item => item.trim()).filter(Boolean)
const cookieFile = argValue('cookie-file', process.env.QISHUI_COOKIE_FILE || '')
const sessionFile = argValue('session-file', process.env.QISHUI_SESSION_FILE || '')
const includeMsToken = !hasFlag('no-mstoken')
const extraQuery = argValue('query', '')
// --url 可覆盖默认的 api.qishui.com/luna/pc/track_v2；--method/--body 用于 POST 端点。
const overrideUrl = argValue('url', '')
const method = argValue('method', 'GET').toUpperCase()
const bodyTemplate = argValue('body', '')

function readCookie() {
  if (!cookieFile) throw new Error('请设置 QISHUI_COOKIE_FILE 或使用 --cookie-file')
  const raw = fs.readFileSync(cookieFile, 'utf8')
  for (const line of raw.split('\n')) {
    const trimmed = line.trim()
    if (trimmed.toLowerCase().startsWith('cookie:')) return trimmed.slice('cookie:'.length).trim()
  }
  throw new Error(`未在 ${cookieFile} 找到 cookie 行`)
}

function readMsToken() {
  try {
    return JSON.parse(fs.readFileSync(sessionFile, 'utf8')).ms_token || ''
  } catch {
    return ''
  }
}

function cookiePairs(cookie) {
  return cookie
    .split(';')
    .map(part => part.trim())
    .filter(Boolean)
    .map(part => {
      const index = part.indexOf('=')
      return { name: part.slice(0, index), value: part.slice(index + 1) }
    })
    .filter(item => item.name && item.value)
}

// 把官方安全页原样提供，只改写 bdms 的加签路径范围。
function patchedSecurityHost() {
  const html = fs.readFileSync(path.join(SECURITY_DIR, 'security_host.html'), 'utf8')
  const before = "bdms: { aid: 386088, paths: ['/passport'] },"
  if (!html.includes(before)) throw new Error('安全页结构与预期不一致，无法改写 bdms 配置')
  const after = `bdms: { aid: 386088, paths: ${JSON.stringify(paths)} },`
  return html.replace(before, after)
}

function startAssetServer(patchedHtml) {
  return new Promise((resolve, reject) => {
    const server = http.createServer((request, response) => {
      const name = path.basename(new URL(request.url, 'http://127.0.0.1').pathname)
      if (name === 'security_host.html') {
        response.setHeader('content-type', 'text/html; charset=utf-8')
        response.end(patchedHtml)
        return
      }
      const file = path.join(SECURITY_DIR, name)
      if (!file.startsWith(SECURITY_DIR) || !fs.existsSync(file)) {
        response.statusCode = 404
        response.end('not found')
        return
      }
      response.setHeader('content-type', 'text/javascript; charset=utf-8')
      fs.createReadStream(file).pipe(response)
    })
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port }))
  })
}

function summarize(label, result) {
  const signed = new URL(result.responseURL || 'about:blank')
  const bogus = signed.searchParams.get('a_bogus') || ''
  let extra = ''
  try {
    const json = JSON.parse(result.body || '{}')
    const track = json.track || {}
    const trackPlayer = json.track_player || json.player_infos?.[0] || {}
    let videoModel = trackPlayer.video_model
    if (typeof videoModel === 'string') {
      try { videoModel = JSON.parse(videoModel) } catch { videoModel = null }
    }
    const qualities = (json.track?.bit_rates || []).map(item => `${item.quality}:${item.size}`).join(',')
    extra = [
      `code=${json.status_code ?? '-'}`,
      json.status_info?.status_msg ? `msg=${json.status_info.status_msg}` : '',
      track.duration ? `duration=${track.duration}` : '',
      Array.isArray(videoModel?.video_list) ? `video_duration=${videoModel.video_duration}` : '',
      qualities ? `bit_rates=[${qualities}]` : '',
    ].filter(Boolean).join(' ')
  } catch {
    /* 非 JSON 响应 */
  }
  console.log(`\n### ${label}`)
  console.log(`  HTTP ${result.status} body=${(result.body || '').length}B a_bogus.len=${bogus.length}`)
  console.log(`  responseURL=${signed.origin}${signed.pathname}`)
  if (extra) console.log(`  ${extra}`)
  if ((result.body || '').length === 0) console.log('  (空响应)')
  return { bogusLength: bogus.length, bodyLength: (result.body || '').length }
}

const { chromium } = loadPlaywright()
const cookie = readCookie()
const msToken = includeMsToken ? readMsToken() : ''
const patchedHtml = patchedSecurityHost()
const { server, port } = await startAssetServer(patchedHtml)
const browser = await chromium.launch({
  executablePath: CHROMIUM,
  args: [
    '--no-sandbox',
    '--disable-dev-shm-usage',
    '--disable-gpu',
    '--disable-extensions',
    '--disable-background-networking',
    '--disable-sync',
    '--renderer-process-limit=1',
    '--disable-web-security',
    '--disable-features=IsolateOrigins,site-per-process,BlockThirdPartyCookies,ThirdPartyStoragePartitioning',
  ],
})

try {
  const context = await browser.newContext()
  await context.addCookies(cookiePairs(cookie).map(({ name, value }) => ({ name, value, url: 'https://api.qishui.com/' })))
  const page = await context.newPage()
  page.on('pageerror', error => console.log(`[pageerror] ${error.message}`))
  await page.goto(`http://127.0.0.1:${port}/security_host.html`, { waitUntil: 'load', timeout: 30000 })
  await page.waitForFunction(() => Boolean(window.bdms), null, { timeout: 30000 })
  if (msToken) {
    await page.evaluate(token => {
      localStorage.setItem('xmst', token)
      localStorage.setItem('xmsty', token)
    }, msToken)
  }
  console.log(`track=${trackId} paths=[${paths.join(',')}] msToken=${msToken ? `${msToken.length}B` : '无'}`)

  const base = new URL(overrideUrl || 'https://api.qishui.com/luna/pc/track_v2')
  const queries = overrideUrl
    ? [[base.pathname, {}]]
    : [
        ['web/channel=pc_web', { track_id: trackId, media_type: 'track', aid: '386088', device_platform: 'web', channel: 'pc_web' }],
        ['web/无 channel', { track_id: trackId, media_type: 'track', aid: '386088', device_platform: 'web' }],
      ]
  for (const [label, query] of queries) {
    const url = new URL(base)
    if (!overrideUrl) for (const [key, value] of Object.entries(query)) url.searchParams.set(key, value)
    if (extraQuery) for (const pair of extraQuery.split('&')) {
      const [key, value] = pair.split('=')
      if (key) url.searchParams.set(key, value ?? '')
    }
    if (msToken) url.searchParams.set('msToken', msToken)
    const body = bodyTemplate ? bodyTemplate.replaceAll('{track}', trackId) : undefined
    const result = await page.evaluate(
      spec => window.__qishuiRequest(spec),
      {
        method,
        url: url.toString(),
        headers: body
          ? { Accept: 'application/json,text/plain,*/*', 'Content-Type': 'application/json; charset=utf-8' }
          : { Accept: 'application/json,text/plain,*/*' },
        body: body ?? null,
        timeout: 60000,
      },
    )
    const stats = summarize(`${method} ${base.pathname} (${label})`, result)
    if (stats.bodyLength > 0) fs.writeFileSync(`/tmp/probe-track-v2-${label.replace(/[^\w]+/g, '_')}.json`, result.body)
  }
} finally {
  await browser.close()
  await new Promise(resolve => server.close(resolve))
}
