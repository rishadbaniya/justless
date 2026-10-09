"""A stdlib Messages API client (non-streaming) with retries, request IDs and key redaction."""

import json
import random
import socket
import time
import urllib.error
import urllib.request

import config

RETRY_STATUS = {429, 500, 502, 503, 504, 529}
FATAL_STATUS = {400, 401, 403, 404, 413}


class APIError(Exception):
    def __init__(self, status, body, request_id=None, attempts=None):
        message = body
        if isinstance(body, dict):
            message = (body.get("error") or {}).get("message") or json.dumps(body)[:500]
        Exception.__init__(self, "HTTP %s: %s" % (status, message))
        self.status = status
        self.body = body
        self.request_id = request_id
        self.attempts = attempts or []

    @property
    def append_only_bug(self):
        text = json.dumps(self.body) if not isinstance(self.body, str) else self.body
        return self.status == 400 and ("block_binding" in text or "bound to a different conversation" in text
                                       or "prefix" in text and "mismatch" in text)


class Client:
    def __init__(self, api_key, base_url=config.API_BASE, version=config.ANTHROPIC_VERSION, betas=None,
                 timeout=900, max_attempts=6, sleep=time.sleep, rng=None):
        self.api_key = api_key
        self.base_url = base_url.rstrip("/")
        self.version = version
        self.betas = list(config.BETAS if betas is None else betas)
        self.timeout = timeout
        self.max_attempts = max_attempts
        self.sleep = sleep
        self.rng = rng or random.Random(config.SEED)

    def headers(self):
        headers = {"content-type": "application/json", "anthropic-version": self.version,
                   "x-api-key": self.api_key}
        if self.betas:
            headers["anthropic-beta"] = ",".join(self.betas)
        return headers

    def redact(self, text):
        if self.api_key and isinstance(text, str):
            return text.replace(self.api_key, "[REDACTED]")
        return text

    def _post(self, path, body):
        data = json.dumps(body).encode("utf-8")
        request = urllib.request.Request(self.base_url + path, data=data, headers=self.headers(), method="POST")
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read()
                return response.status, json.loads(raw.decode("utf-8")), dict(response.headers)
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                parsed = json.loads(raw.decode("utf-8"))
            except ValueError:
                parsed = self.redact(raw.decode("utf-8", "replace"))
            return error.code, parsed, dict(error.headers or {})

    def create(self, body):
        """POST /v1/messages. Returns (response, meta). Raises APIError after retries or on a fatal status."""
        attempts = []
        wait_total = 0.0
        for attempt in range(1, self.max_attempts + 1):
            started = time.time()
            status = None
            payload = None
            headers = {}
            error_kind = None
            try:
                status, payload, headers = self._post("/v1/messages", body)
            except (urllib.error.URLError, socket.timeout, ConnectionError, OSError) as error:
                error_kind = type(error).__name__
            latency = time.time() - started
            lower = {k.lower(): v for k, v in headers.items()}
            record = {"attempt": attempt, "status": status, "latency_s": round(latency, 3),
                      "request_id": lower.get("request-id"), "error": error_kind,
                      "prefix_mismatch_header": lower.get("anthropic-thinking-prefix-mismatch"),
                      "maybe_billed": error_kind in ("timeout", "socket.timeout") or isinstance(error_kind, str)
                      and "imeout" in error_kind}
            attempts.append(record)
            if status == 200:
                meta = {"attempts": attempts, "retry_wait_s": round(wait_total, 3), "latency_s": round(latency, 3),
                        "request_id": record["request_id"]}
                return payload, meta
            if status in FATAL_STATUS:
                raise APIError(status, payload, record["request_id"], attempts)
            if status is not None and status not in RETRY_STATUS:
                raise APIError(status, payload, record["request_id"], attempts)
            if attempt == self.max_attempts:
                raise APIError(status or 0, payload or error_kind, record["request_id"], attempts)
            delay = min(64.0, 2.0 * (2 ** (attempt - 1))) * (0.5 + self.rng.random())
            retry_after = lower.get("retry-after")
            if retry_after:
                try:
                    delay = max(delay, float(retry_after))
                except ValueError:
                    pass
            wait_total += delay
            self.sleep(delay)
        raise APIError(0, "unreachable")

    def count_tokens(self, model, text):
        status, payload, _ = self._post("/v1/messages/count_tokens", {
            "model": model, "messages": [{"role": "user", "content": text}], "tools": []})
        if status != 200:
            raise APIError(status, payload)
        return int(payload["input_tokens"])
