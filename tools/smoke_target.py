"""插件冒烟用的本地目标服务（只在本机回环上跑，用完即关）。

- HTTP 18080：提供若干反射形态（正文 / 属性 / 已编码）+ 带版本头，供
  `xss_probe` / `http_version_cve` 打。
- RESP 16379：模拟一个开了 ACL 密码的 Redis 6，供 `weak_password_redis` 打。

不是为了测试框架，而是为了证明"插件 → 宿主 → 真实 socket"这条链路通了。
"""

import html
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

PASSWORD = "s3cret"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _q(self):
        u = urlparse(self.path)
        return u.path, (parse_qs(u.query).get("q") or [""])[0]

    def do_GET(self):  # noqa: N802
        path, q = self._q()
        if path == "/reflect":
            body = f"<html><body><p>你搜的是：{q}</p></body></html>"
        elif path == "/safe":
            body = f"<html><body><p>你搜的是：{html.escape(q)}</p></body></html>"
        elif path == "/attr":
            body = f'<html><body><input name="q" value="{q}"></body></html>'
        else:
            body = "<html><body><h1>home</h1></body></html>"
        raw = body.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        # 故意放一个旧版本指纹：nginx 1.18.0 命中 CVE-2021-23017
        self.send_header("Server", "nginx/1.18.0")
        self.send_header("X-Powered-By", "PHP/7.4.3")
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_a):
        pass


def redis_conn(conn):
    buf = b""
    authed = False
    while True:
        try:
            data = conn.recv(4096)
        except OSError:
            return
        if not data:
            return
        buf += data
        while True:
            args, buf = parse_resp(buf)
            if args is None:
                break
            cmd = [a.decode("utf-8", "replace") for a in args]
            name = (cmd[0] if cmd else "").upper()
            if name == "PING":
                reply = b"+PONG\r\n" if authed else b"-NOAUTH Authentication required.\r\n"
            elif name == "AUTH":
                if len(cmd) == 3:
                    authed = cmd[1] == "default" and cmd[2] == PASSWORD
                    reply = b"+OK\r\n" if authed else b"-WRONGPASS invalid username-password pair\r\n"
                elif len(cmd) == 2:
                    authed = cmd[1] == PASSWORD
                    reply = b"+OK\r\n" if authed else b"-WRONGPASS invalid username-password pair\r\n"
                else:
                    reply = b"-ERR wrong number of arguments for 'auth' command\r\n"
            elif name == "INFO":
                if not authed:
                    reply = b"-NOAUTH Authentication required.\r\n"
                else:
                    info = "# Server\r\nredis_version:6.2.6\r\nos:Linux\r\n"
                    reply = b"$%d\r\n%s\r\n" % (len(info), info.encode())
            else:
                reply = b"-ERR unknown command\r\n"
            try:
                conn.sendall(reply)
            except OSError:
                return


def parse_resp(buf):
    """极简 RESP 解析：只认 *N / $len 两种前缀。"""
    if not buf.startswith(b"*"):
        return None, buf
    try:
        head, rest = buf.split(b"\r\n", 1)
        n = int(head[1:])
    except (ValueError, IndexError):
        return None, buf
    args = []
    for _ in range(n):
        if not rest.startswith(b"$"):
            return None, buf
        try:
            h, r2 = rest.split(b"\r\n", 1)
            ln = int(h[1:])
        except (ValueError, IndexError):
            return None, buf
        if len(r2) < ln + 2:
            return None, buf
        args.append(r2[:ln])
        rest = r2[ln + 2:]
    return args, rest


def serve_redis():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 16379))
    s.listen(32)
    print("redis on 127.0.0.1:16379", flush=True)
    while True:
        conn, _ = s.accept()
        threading.Thread(target=redis_conn, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    threading.Thread(target=serve_redis, daemon=True).start()
    httpd = ThreadingHTTPServer(("127.0.0.1", 18080), Handler)
    print("http on 127.0.0.1:18080", flush=True)
    httpd.serve_forever()
