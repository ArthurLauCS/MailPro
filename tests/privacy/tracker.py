"""隐私测试的本地计数服务器。

    python tracker.py serve [log]     在 127.0.0.1:8765 记录每个请求（打印并追加到 log）
    python tracker.py html <eml> <out> 取出邮件的 HTML 正文，给无防护的浏览器做对照

测试邮件里的跟踪地址都是 http://127.0.0.1:8765/v/<手段名>。
"""
import email
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GIF = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;"


def serve(log):
    class Hit(BaseHTTPRequestHandler):
        def do_GET(self):
            line = f"{self.command} {self.path}"
            print(line, flush=True)
            if log:
                with open(log, "a", encoding="utf-8") as f:
                    f.write(line + "\n")
            self.send_response(200)
            self.send_header("Content-Type", "image/gif")
            self.end_headers()
            self.wfile.write(GIF)

        do_POST = do_HEAD = do_GET

        def log_message(self, *args):
            pass

    ThreadingHTTPServer(("127.0.0.1", 8765), Hit).serve_forever()


def html(eml, out):
    msg = email.message_from_binary_file(open(eml, "rb"))
    part = next(p for p in msg.walk() if p.get_content_type() == "text/html")
    open(out, "wb").write(part.get_payload(decode=True))


if __name__ == "__main__":
    if sys.argv[1] == "serve":
        serve(sys.argv[2] if len(sys.argv) > 2 else None)
    else:
        html(sys.argv[2], sys.argv[3])
