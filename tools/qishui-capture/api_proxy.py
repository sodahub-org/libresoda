#!/usr/bin/env python3
"""汽水 PC 客户端「应用签名凭证」抓包代理（HTTPS 终止 + 透传）。

用途：官方客户端对 api.qishui.com 的请求带着 `x-helios` / `x-medusa` 应用级
签名头（由 mssdk 原生库生成，纯软件侧无法复刻）。本代理把客户端流量引到自己
的 HTTPS 端口上，记录这些头，再把请求原样转发给真实上游，因此客户端照常工作。

配套步骤见 docs/FULL-QUALITY-STREAM.md：
  1) openssl 生成 CA 与 api.qishui.com 证书；
  2) 在抓包机（Linux）上运行本代理：sudo python3 api_proxy.py --port 443 ...
  3) 在运行客户端的机器上把 api.qishui.com 指向抓包机，并把 CA 导入信任区；
  4) 重启客户端，任意操作一次，然后从日志里取 x-helios / x-medusa / device_id。

日志默认脱敏：Cookie 只记录字段名与长度。`x-helios` / `x-medusa` 属于**逐请求**
签名（每次请求都不一样），所以真正要留下来的是客户端 Cookie + 设备指纹，
它们会被写到 `--secret-log`（默认 `/tmp/qishui-capture-secrets.log`，权限 600）。

仅用于本机自用抓包，请勿用于规避版权保护或分发他人凭证。
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import re
import socket
import ssl
import sys
import threading
import time
from urllib.parse import urlsplit

MAX_BODY = 8 * 1024 * 1024


def cookie_summary(value: str) -> str:
    pairs = [item.strip() for item in value.split(";") if item.strip()]
    names = [item.split("=", 1)[0] for item in pairs]
    return f"<redacted; {len(value)} chars; keys={','.join(names)}>"


def read_until(sock: socket.socket, terminator: bytes, chunk: int = 4096) -> bytes:
    data = b""
    while terminator not in data:
        piece = sock.recv(chunk)
        if not piece:
            break
        data += piece
        if len(data) > MAX_BODY:
            break
    return data


def parse_headers(raw: bytes) -> tuple[str, str, str, list[tuple[str, str]]]:
    text = raw.decode("iso-8859-1")
    lines = text.split("\r\n")
    request_line = lines[0]
    parts = request_line.split(" ")
    method = parts[0] if parts else ""
    target = parts[1] if len(parts) > 1 else "/"
    version = parts[2] if len(parts) > 2 else "HTTP/1.1"
    headers: list[tuple[str, str]] = []
    for line in lines[1:]:
        if not line or ":" not in line:
            continue
        name, value = line.split(":", 1)
        headers.append((name.strip(), value.strip()))
    return method, target, version, headers


class CaptureProxy:
    def __init__(self, options: argparse.Namespace) -> None:
        self.options = options
        self.lock = threading.Lock()
        self.log_file = open(options.log, "a", buffering=1, encoding="utf-8")
        self.secret_file = None
        if options.secret_log:
            self.secret_file = open(options.secret_log, "a", buffering=1, encoding="utf-8")
            os.chmod(options.secret_log, 0o600)

    def log(self, record: dict) -> None:
        line = json.dumps(record, ensure_ascii=False)
        with self.lock:
            self.log_file.write(line + "\n")
            print(line[:400], flush=True)

    def handle(self, raw_sock: socket.socket, addr: tuple[str, int]) -> None:
        try:
            tls = self.options.ssl_context.wrap_socket(raw_sock, server_side=True)
        except Exception as error:  # 客户端可能直连探测，直接放弃
            self.log({"event": "tls_error", "peer": f"{addr[0]}:{addr[1]}", "error": str(error)})
            raw_sock.close()
            return

        with tls:
            peer = f"{addr[0]}:{addr[1]}"
            while True:
                head = read_until(tls, b"\r\n\r\n")
                if not head:
                    return
                header_blob, _, rest = head.partition(b"\r\n\r\n")
                method, target, version, headers = parse_headers(header_blob)
                lowered = {name.lower(): value for name, value in headers}
                body = rest
                if "content-length" in lowered:
                    need = int(lowered["content-length"])
                    while len(body) < need:
                        piece = tls.recv(min(65536, need - len(body)))
                        if not piece:
                            break
                        body += piece
                elif lowered.get("transfer-encoding", "").lower() == "chunked":
                    body = read_chunked(tls, body)

                record = {
                    "event": "request",
                    "at": time.time(),
                    "peer": peer,
                    "method": method,
                    "target": target,
                    "headers": {
                        name: (cookie_summary(value) if name.lower() == "cookie" else value)
                        for name, value in headers
                    },
                    "body_bytes": len(body),
                    "body_preview": body[:600].decode("utf-8", "replace"),
                }
                self.log(record)
                if self.secret_file is not None:
                    lowered_headers = {name.lower(): value for name, value in headers}
                    if "cookie" in lowered_headers:
                        self.secret_file.write(
                            json.dumps(
                                {
                                    "at": record["at"],
                                    "method": method,
                                    "target": target,
                                    "cookie": lowered_headers["cookie"],
                                    "device_query": target,
                                    "x_helios": lowered_headers.get("x-helios", ""),
                                    "x_medusa": lowered_headers.get("x-medusa", ""),
                                    "user_agent": lowered_headers.get("user-agent", ""),
                                },
                                ensure_ascii=False,
                            )
                            + "\n"
                        )

                try:
                    status, resp_headers, payload = self.forward(method, target, headers, body)
                except Exception as error:
                    payload = json.dumps({"proxy_error": str(error)}).encode()
                    status, resp_headers = 502, [("Content-Type", "application/json")]
                self.log(
                    {
                        "event": "response",
                        "at": time.time(),
                        "method": method,
                        "target": target.split("?")[0],
                        "status": status,
                        "body_bytes": len(payload),
                        "body_preview": payload[:400].decode("utf-8", "replace"),
                    }
                )
                if self.options.dump_responses and len(payload) > 0:
                    name = re.sub(r"[^A-Za-z0-9_.-]+", "_", f"{int(time.time() * 1000)}-{target.split('?')[0]}")
                    with open(os.path.join(self.options.dump_responses, f"{name}.bin"), "wb") as handle:
                        handle.write(payload)
                response = [f"HTTP/1.1 {status} {http.client.responses.get(status, '')}".strip()]
                response += [f"{name}: {value}" for name, value in resp_headers]
                response += ["Connection: keep-alive", "", ""]
                tls.sendall("\r\n".join(response).encode("iso-8859-1"))
                tls.sendall(payload)

                if lowered.get("connection", "").lower() == "close":
                    return

    def forward(self, method: str, target: str, headers: list[tuple[str, str]], body: bytes):
        upstream = self.options.upstream
        path = target
        if target.startswith("http://") or target.startswith("https://"):
            split = urlsplit(target)
            upstream = split.netloc
            path = split.path + (f"?{split.query}" if split.query else "")
        connection = http.client.HTTPSConnection(upstream, self.options.upstream_port, timeout=30)
        forwarded = [
            (name, value)
            for name, value in headers
            if name.lower() not in {"host", "connection", "content-length", "accept-encoding", "transfer-encoding"}
        ]
        forwarded.append(("Host", upstream))
        forwarded.append(("Accept-Encoding", "identity"))
        if body:
            forwarded.append(("Content-Length", str(len(body))))
        connection.request(method, path, body=body or None, headers=dict(forwarded))
        response = connection.getresponse()
        payload = response.read(MAX_BODY)
        resp_headers = [(name, value) for name, value in response.getheaders()]
        connection.close()
        return response.status, resp_headers, payload


def read_chunked(sock: socket.socket, buffered: bytes) -> bytes:
    data = buffered
    while b"\r\n" not in data:
        piece = sock.recv(4096)
        if not piece:
            return data
        data += piece
    return data


def build_ssl_context(cert: str, key: str) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certfile=cert, keyfile=key)
    context.set_alpn_protocols(["http/1.1"])
    return context


def main() -> int:
    parser = argparse.ArgumentParser(description="汽水客户端签名头抓包代理")
    parser.add_argument("--port", type=int, default=443)
    parser.add_argument("--bind", default="0.0.0.0")
    parser.add_argument("--cert", required=True, help="api.qishui.com 证书（含链）")
    parser.add_argument("--key", required=True)
    parser.add_argument("--upstream", default="api.qishui.com")
    parser.add_argument("--upstream-port", type=int, default=443)
    parser.add_argument("--log", default="/tmp/qishui-capture.log")
    parser.add_argument(
        "--secret-log",
        default="/tmp/qishui-capture-secrets.log",
        help="完整 Cookie / 签名的落盘位置（0600）；置空字符串可关闭",
    )
    parser.add_argument(
        "--dump-responses",
        default="",
        help="把每个响应体完整写到该目录（用于确认服务端到底下发了试听还是整曲）",
    )
    options = parser.parse_args()
    options.ssl_context = build_ssl_context(options.cert, options.key)

    proxy = CaptureProxy(options)
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((options.bind, options.port))
    listener.listen(64)
    print(f"capture proxy listening on {options.bind}:{options.port} → {options.upstream}", flush=True)
    while True:
        raw_sock, addr = listener.accept()
        threading.Thread(target=proxy.handle, args=(raw_sock, addr), daemon=True).start()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
