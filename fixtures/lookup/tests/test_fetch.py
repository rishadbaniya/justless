from scripts.fetch import download


def test_download_retries(monkeypatch):
    calls = []
    monkeypatch.setattr("urllib.request.urlopen", lambda url: calls.append(url))
    download("https://example.invalid/file", tries=1)
    assert calls == ["https://example.invalid/file"]
