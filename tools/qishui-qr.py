#!/usr/bin/env python3
"""汽水扫码登录（纯 HTTP 版，按 Meting-API 的 providers/qishui/qr.js 复刻）。

与参考实现的差异：本工具不发签名（实测 get_qrcode / check_qrconnect 在无签名下
返回 error_code=0），但**完整保留会话语义**：device_id / install_id / msToken /
verify_portrait_id 全程一致，create 阶段下发的 cookie 会带到后续所有请求。

用法：
    python3 qishui-qr.py create --out DIR        # 生成二维码（写 qr-url.txt / session.json）
    python3 qishui-qr.py check  --dir DIR        # 轮询（可反复执行；读到会话即写出 cookie.txt）
可选项：
    --interval 秒（默认 6）  --attempts 次数（默认 5）
"""

import argparse
import hashlib
import http.cookiejar
import json
import os
import random
import sys
import time
import urllib.parse
import urllib.request

API_BASE = "https://api.qishui.com"
AID = "386088"
UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) SodaMusic/3.2.1 Chrome/136.0.7103.59 Electron/36.4.0 Safari/537.36"
)
SESSION_KEYS = ("sessionid", "sessionid_ss", "sid_guard", "sid_tt")


def rand_digits(length: int) -> str:
    out = str(random.randint(1, 8))
    while len(out) < length:
        out += str(random.randint(0, 9))
    return out


def new_session() -> dict:
    return {
        "device_id": rand_digits(16),
        "install_id": rand_digits(15),
        "ms_token": urllib.parse.quote(
            os.urandom(66).hex(), safe=""
        ).replace("%", "")[:88] + "==",
        "verify_portrait_id": f"{rand_digits(8)}-{rand_digits(4)}-{rand_digits(4)}-{rand_digits(4)}-{rand_digits(11)}.login",
        "token": "",
        "expire_time": 0,
        "created_ms": int(time.time() * 1000),
    }


def common_params(session: dict, biz_trace_id: str) -> dict:
    return {
        "passport_jssdk_version": "2.4.13",
        "passport_jssdk_type": "normal",
        "is_from_ttaccountsdk": "1",
        "aid": AID,
        "language": "zh",
        "account_sdk_source": "web",
        "p_js_v": "2.4.13",
        "p_js_t": "pro",
        "p_zt": "3.3.5",
        "p_ver": "1.0.29",
        "request_host": "app%3A%2F%2Fresources",
        "p_bd": "1.0.0.41",
        "biz_trace_id": biz_trace_id,
        "is_new_login": "1",
        "is_from_iesaccountsaas": "1",
        "device_id": session["device_id"],
        "install_id": session["install_id"],
        "did": session["device_id"],
        "iid": session["install_id"],
        "device_platform": "PC",
        "version_code": "3.5.2",
        "account_sdk_source_info": "00",
        "msToken": session["ms_token"],
    }


class Client:
    def __init__(self, cookie_file: str):
        self.jar = http.cookiejar.MozillaCookieJar(cookie_file)
        if os.path.exists(cookie_file):
            try:
                self.jar.load(ignore_discard=True, ignore_expires=True)
            except Exception:
                pass
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def save(self, cookie_file: str) -> None:
        try:
            self.jar.save(cookie_file, ignore_discard=True, ignore_expires=True)
        except Exception:
            pass

    def request(self, session: dict, method: str, path: str, extra: dict, body: dict | None):
        biz = f"{random.getrandbits(32):08x}"
        params = common_params(session, biz)
        params.update(extra)
        url = f"{API_BASE}{path}?{urllib.parse.urlencode(params)}"
        data = urllib.parse.urlencode(body).encode() if body else None
        request = urllib.request.Request(url, data=data, method=method)
        request.add_header("Accept", "application/json, text/javascript")
        request.add_header("User-Agent", UA)
        request.add_header("x-tt-passport-verify-portrait", session["verify_portrait_id"])
        request.add_header("x-tt-passport-trace-id", biz)
        if data is not None:
            request.add_header("Content-Type", "application/x-www-form-urlencoded")
            request.add_header("x-ss-stub", hashlib.md5(data).hexdigest().upper())
        with self.opener.open(request, timeout=30) as response:
            payload = json.loads(response.read().decode("utf-8", "replace"))
        session["cookies"] = "; ".join(f"{c.name}={c.value}" for c in self.jar)
        return payload


class SignerClient:
    """把请求交给本地签名服务（tools/qishui-signer）执行。

    参考实现（Meting-API）就是这样：请求由跑着官方安全组件的页面发出，
    Cookie 保存在那个浏览器上下文里，因此 create 与 check 必须共用同一个服务实例。
    """

    def __init__(self, endpoint: str):
        self.endpoint = endpoint
        self.cookies: list = []

    def save(self, cookie_file: str) -> None:
        pairs = [
            f"{c.get('name')}={c.get('value')}"
            for c in self.cookies
            if c.get("name") and c.get("value")
        ]
        with open(cookie_file, "w", encoding="utf-8") as handle:
            handle.write("cookie: " + "; ".join(pairs) + "\n")

    def request(self, session: dict, method: str, path: str, extra: dict, body: dict | None):
        biz = f"{random.getrandbits(32):08x}"
        params = common_params(session, biz)
        params.update(extra)
        url = f"{API_BASE}{path}?{urllib.parse.urlencode(params)}"
        data = urllib.parse.urlencode(body) if body else None
        headers = {
            "Accept": "application/json, text/javascript",
            "User-Agent": UA,
            "x-tt-passport-verify-portrait": session["verify_portrait_id"],
            "x-tt-passport-trace-id": biz,
        }
        if data is not None:
            headers["Content-Type"] = "application/x-www-form-urlencoded"
            headers["x-ss-stub"] = hashlib.md5(data.encode()).hexdigest().upper()
        spec = {
            "method": method,
            "url": url,
            "headers": headers,
            "body": data,
            "msToken": session["ms_token"],
        }
        request = urllib.request.Request(
            self.endpoint,
            data=json.dumps(spec).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=300) as response:
            result = json.loads(response.read().decode("utf-8", "replace"))
        if not result.get("ok"):
            raise RuntimeError(result.get("error") or "签名服务返回失败")
        self.cookies = result.get("cookies") or []
        session["signed_url"] = result.get("responseURL", "")
        session["cookies"] = "; ".join(
            f"{c.get('name')}={c.get('value')}"
            for c in self.cookies
            if c.get("name") and c.get("value")
        )
        return json.loads(result.get("body") or "{}")


def load_session(directory: str) -> dict:
    with open(os.path.join(directory, "session.json"), encoding="utf-8") as handle:
        return json.load(handle)


def save_session(directory: str, session: dict) -> None:
    with open(os.path.join(directory, "session.json"), "w", encoding="utf-8") as handle:
        json.dump(session, handle, ensure_ascii=False, indent=2)


def cmd_create(args) -> int:
    os.makedirs(args.out, exist_ok=True)
    session = new_session()
    client = (
        SignerClient(args.signer)
        if args.signer
        else Client(os.path.join(args.out, "cookies.txt"))
    )
    payload = client.request(
        session,
        "GET",
        "/passport/web/get_qrcode/",
        {"next": API_BASE, "need_logo": "false", "need_short_url": "false", "is_new_login": "1"},
        None,
    )
    data = payload.get("data") or {}
    token = (data.get("token") or "").strip()
    if payload.get("message") != "success" or int(data.get("error_code") or 0) != 0 or not token:
        print("创建失败:", json.dumps(payload, ensure_ascii=False)[:300])
        return 1
    session["token"] = token
    session["expire_time"] = int(data.get("expire_time") or 0)
    save_session(args.out, session)
    client.save(os.path.join(args.out, "cookies.txt"))
    scan_url = "https://bff-pc.qishui.com/light/invoke/scan_login?" + urllib.parse.urlencode(
        {"token": token, "os": "Windows", "computer_name": "libresoda"}
    )
    with open(os.path.join(args.out, "qr-url.txt"), "w", encoding="utf-8") as handle:
        handle.write(scan_url + "\n")
    print("token:", token)
    print("过期时间戳:", session["expire_time"])
    print("扫码地址:", scan_url)
    print("会话目录:", args.out)
    return 0


def cmd_check(args) -> int:
    session = load_session(args.dir)
    client = (
        SignerClient(args.signer)
        if args.signer
        else Client(os.path.join(args.dir, "cookies.txt"))
    )
    body = {
        "need_logo": "false",
        "need_short_url": "false",
        "is_frontier": "true",
        "token": session["token"],
        "is_new_login": "1",
        "next": API_BASE,
    }
    for attempt in range(1, args.attempts + 1):
        payload = client.request(session, "POST", "/passport/web/check_qrconnect/", {}, body)
        client.save(os.path.join(args.dir, "cookies.txt"))
        data = payload.get("data") or {}
        status = data.get("status")
        error_code = data.get("error_code")
        cookie_text = session.get("cookies", "")
        has_session = any(f"{key}=" in cookie_text for key in SESSION_KEYS)
        print(
            f"#{attempt} status={status} error_code={error_code} "
            f"desc={(data.get('description') or '')[:40]} 会话={'有' if has_session else '无'}",
            flush=True,
        )
        if has_session:
            with open(os.path.join(args.dir, "cookie.txt"), "w", encoding="utf-8") as handle:
                handle.write("cookie: " + cookie_text + "\n")
            print("★ 登录成功，Cookie 已写入", os.path.join(args.dir, "cookie.txt"))
            return 0
        if error_code == 2 or status in ("expired", "expire", "timeout"):
            print("二维码已过期")
            return 1
        time.sleep(args.interval)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    create = sub.add_parser("create")
    create.add_argument("--out", required=True)
    create.add_argument("--signer", default="", help="签名服务地址（如 http://127.0.0.1:8799/request）")
    create.set_defaults(func=cmd_create)
    check = sub.add_parser("check")
    check.add_argument("--dir", required=True)
    check.add_argument("--interval", type=int, default=6)
    check.add_argument("--attempts", type=int, default=5)
    check.add_argument("--signer", default="", help="签名服务地址（如 http://127.0.0.1:8799/request）")
    check.set_defaults(func=cmd_check)
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
