"""A logging forwarder for the real Claude Code anchor.

Listens on 127.0.0.1, forwards every request unchanged to the upstream (api.anthropic.com by
default), streams the response back, and logs each /v1/messages request's model, request-id
and usage, including subagent and side requests. Credentials are forwarded but never logged.
"""

import http.server
import json
import threading
import time
import urllib.error
import urllib.request

HOP = {"host", "content-length", "connection", "accept-encoding", "transfer-encoding", "keep-alive"}
SECRET = {"x-api-key", "authorization", "cookie", "proxy-authorization"}


def sse_usage(raw):
    """Merge usage from message_start and message_delta events of an SSE body."""
    usage = {}
    model = None
    stop = None
    for line in raw.decode("utf-8", "replace").split("\n"):
        if not line.startswith("data: "):
            continue
        try:
            event = json.loads(line[6:])
        except ValueError:
            continue
        if event.get("type") == "message_start":
            message = event.get("message") or {}
            model = message.get("model")
            usage.update(message.get("usage") or {})
        elif event.get("type") == "message_delta":
            usage.update({k: v for k, v in (event.get("usage") or {}).items() if v is not None})
            stop = (event.get("delta") or {}).get("stop_reason")
    return model, usage, stop


class Proxy:
    def __init__(self, upstream, log_path, port=0):
        self.upstream = upstream.rstrip("/")
        self.log_path = str(log_path)
        self.lock = threading.Lock()
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), self._handler())
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]

    @property
    def base_url(self):
        return "http://127.0.0.1:%d" % self.port

    def start(self):
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def log(self, row):
        with self.lock:
            with open(self.log_path, "a") as handle:
                handle.write(json.dumps(row) + "\n")

    def _handler(self):
        proxy = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _forward(self, method):
                length = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(length) if length else None
                headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
                request = urllib.request.Request(proxy.upstream + self.path, data=body, headers=headers, method=method)
                started = time.time()
                try:
                    response = urllib.request.urlopen(request, timeout=900)
                    status = response.status
                except urllib.error.HTTPError as error:
                    response = error
                    status = error.code
                raw = response.read()
                self.send_response(status)
                for key, value in response.headers.items():
                    if key.lower() not in HOP:
                        self.send_header(key, value)
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
                path = self.path.split("?")[0]
                if path.endswith("/v1/messages"):
                    try:
                        sent = json.loads(body.decode("utf-8")) if body else {}
                    except ValueError:
                        sent = {}
                    ctype = response.headers.get("content-type", "")
                    if "event-stream" in ctype:
                        model, usage, stop = sse_usage(raw)
                    else:
                        try:
                            parsed = json.loads(raw.decode("utf-8"))
                        except ValueError:
                            parsed = {}
                        model, usage, stop = parsed.get("model"), parsed.get("usage") or {}, parsed.get("stop_reason")
                    proxy.log({"t": started, "latency_s": round(time.time() - started, 3), "status": status,
                               "path": path, "request_id": response.headers.get("request-id"),
                               "model": model or sent.get("model"), "usage": usage, "stop_reason": stop,
                               "effort": (sent.get("output_config") or {}).get("effort"),
                               "max_tokens": sent.get("max_tokens"), "tools": [t.get("name") for t in sent.get("tools") or []],
                               "messages": len(sent.get("messages") or []),
                               "has_claude_md": "Contents of " in json.dumps(sent.get("messages") or [])[:200000],
                               "headers": {k: v for k, v in self.headers.items() if k.lower() not in SECRET
                                           and k.lower().startswith(("anthropic", "x-claude", "user-agent"))}})

            def do_POST(self):
                self._forward("POST")

            def do_GET(self):
                self._forward("GET")

        return Handler


def main(argv=None):
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", default="https://api.anthropic.com")
    parser.add_argument("--log", required=True)
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args(argv)
    proxy = Proxy(args.upstream, args.log, args.port)
    print(proxy.base_url, flush=True)
    proxy.httpd.serve_forever()


if __name__ == "__main__":
    main()
