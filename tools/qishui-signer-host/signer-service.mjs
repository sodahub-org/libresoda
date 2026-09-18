#!/usr/bin/env node
// 汽水签名服务（部署在能跑 mssdk 的机器上：Windows / macOS）。
//
// 分离架构：
//
//   [libresoda / 第三方客户端(Linux)] --HTTP--> [本服务(Windows)] --> [mssdk/官方客户端]
//        POST /sign {"url","method","body","ts_ms"}
//        <-- {"headers":{"x-helios":"..","x-medusa":".."}}
//
// 本文件只负责「网络边界 + 契约」，签名后端可插拔：
//
//   backend=command  调外部桥接器：stdin 进 SignRequest、stdout 出 SignResponse
//                    （与 libresoda 的 CommandSignature 完全同一份契约）
//   backend=http     把请求转给另一个签名服务（多级部署 / 中转）
//   backend=echo     回显式假签名，仅用于联调链路（拿不到真整曲流）
//
// 环境变量：
//   PORT                      监听端口，默认 8899
//   BIND                      监听地址，默认 0.0.0.0（建议改成内网地址）
//   QISHUI_SIGNER_BACKEND     command | http | echo
//   QISHUI_SIGNER_COMMAND     后端桥接器命令行（backend=command）
//   QISHUI_SIGNER_UPSTREAM    上游签名服务地址（backend=http）
//   QISHUI_SIGNER_TOKEN       可选：调用方需带 Authorization: Bearer <token>
//   QISHUI_SIGNER_TIMEOUT_MS  后端超时，默认 8000
//
// 启动：node signer-service.mjs
// 自检：node signer-service.mjs --selftest

import http from 'node:http'
import { spawn } from 'node:child_process'

const PORT = Number(process.env.PORT || 8899)
const BIND = process.env.BIND || '0.0.0.0'
const BACKEND = (process.env.QISHUI_SIGNER_BACKEND || 'echo').trim()
const COMMAND = (process.env.QISHUI_SIGNER_COMMAND || '').trim()
const UPSTREAM = (process.env.QISHUI_SIGNER_UPSTREAM || '').trim()
const TOKEN = (process.env.QISHUI_SIGNER_TOKEN || '').trim()
const TIMEOUT_MS = Number(process.env.QISHUI_SIGNER_TIMEOUT_MS || 8000)

const redact = value => {
    const text = String(value ?? '')
    return text.length <= 8 ? `<${text.length} chars>` : `<${text.length} chars>`
}

const summarise = result => ({
    headers: Object.fromEntries(Object.entries(result?.headers || {}).map(([name, value]) => [name, redact(value)])),
    ms_token: result?.ms_token ? redact(result.ms_token) : '',
    a_bogus: result?.a_bogus ? redact(result.a_bogus) : '',
})

function runCommandBackend(payload) {
    return new Promise((resolve, reject) => {
        if (!COMMAND) {
            reject(new Error('QISHUI_SIGNER_COMMAND 未配置'))
            return
        }
        const child = spawn(COMMAND, { shell: true, windowsHide: true })
        let stdout = ''
        let stderr = ''
        const timer = setTimeout(() => {
            child.kill()
            reject(new Error(`桥接器超时（${TIMEOUT_MS}ms）`))
        }, TIMEOUT_MS)
        child.stdout.on('data', chunk => { stdout += chunk })
        child.stderr.on('data', chunk => { stderr += chunk })
        child.on('error', error => { clearTimeout(timer); reject(error) })
        child.on('close', code => {
            clearTimeout(timer)
            if (code !== 0) {
                reject(new Error(`桥接器退出码 ${code}: ${stderr.trim().slice(0, 200)}`))
                return
            }
            try {
                resolve(JSON.parse(stdout || '{}'))
            } catch (error) {
                reject(new Error(`桥接器输出不是 JSON: ${error.message}`))
            }
        })
        child.stdin.end(JSON.stringify(payload))
    })
}

async function runHttpBackend(payload) {
    if (!UPSTREAM) throw new Error('QISHUI_SIGNER_UPSTREAM 未配置')
    const response = await fetch(UPSTREAM, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...(TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {}) },
        body: JSON.stringify(payload),
        signal: AbortSignal.timeout(TIMEOUT_MS),
    })
    if (!response.ok) throw new Error(`上游签名服务 HTTP ${response.status}`)
    return response.json()
}

async function sign(payload) {
    switch (BACKEND) {
        case 'command':
            return runCommandBackend(payload)
        case 'http':
            return runHttpBackend(payload)
        case 'echo':
            // 只为联调：把请求特征塞进假签名，便于确认链路真的到了本服务。
            return {
                headers: {
                    'x-helios': `echo-helios-${payload.method || 'GET'}-${String(payload.url || '').length}`,
                    'x-medusa': `echo-medusa-${String(payload.body || '').length}`,
                },
            }
        default:
            throw new Error(`未知的 QISHUI_SIGNER_BACKEND: ${BACKEND}`)
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

const server = http.createServer(async (request, response) => {
    const url = new URL(request.url || '/', `http://127.0.0.1:${PORT}`)
    const send = (status, payload) => {
        response.statusCode = status
        response.setHeader('Content-Type', 'application/json; charset=utf-8')
        response.end(JSON.stringify(payload))
    }
    try {
        if (url.pathname === '/healthz') {
            send(200, { ok: true, backend: BACKEND, auth: Boolean(TOKEN) })
            return
        }
        if (url.pathname !== '/sign' || request.method !== 'POST') {
            send(404, { ok: false, error: 'not found' })
            return
        }
        if (TOKEN && request.headers.authorization !== `Bearer ${TOKEN}`) {
            send(401, { ok: false, error: 'unauthorized' })
            return
        }
        const payload = JSON.parse((await readBody(request)) || '{}')
        if (!payload.url) {
            send(400, { ok: false, error: 'url 不能为空' })
            return
        }
        const result = await sign(payload)
        // 只打印"请求形状 + 签名长度"，避免把凭据写进日志。
        console.log(JSON.stringify({
            at: new Date().toISOString(),
            method: payload.method || 'GET',
            path: String(payload.url).split('?')[0],
            bodyBytes: String(payload.body || '').length,
            result: summarise(result),
        }))
        send(200, { ok: true, ...result })
    } catch (error) {
        console.error(`[signer] ${error.message}`)
        send(502, { ok: false, error: error.message })
    }
})

if (process.argv.includes('--selftest')) {
    sign({ url: 'https://api.qishui.com/luna/pc/track_v2?aid=386088', method: 'POST', body: '{"track_id":"1"}' })
        .then(result => {
            console.log(JSON.stringify({ backend: BACKEND, ok: true, result: summarise(result) }, null, 2))
        })
        .catch(error => {
            console.error(JSON.stringify({ backend: BACKEND, ok: false, error: error.message }, null, 2))
            process.exitCode = 1
        })
} else {
    server.listen(PORT, BIND, () => {
        console.log(`qishui signer service on http://${BIND}:${PORT} (backend=${BACKEND})`)
    })
}
