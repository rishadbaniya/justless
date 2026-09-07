import time
import urllib.request


def download(url, tries=3):
    """Download a file, retrying on failure."""
    for attempt in range(tries):
        try:
            return urllib.request.urlopen(url).read()
        except OSError:
            time.sleep(2 ** attempt)
    raise RuntimeError(f"gave up on {url}")
