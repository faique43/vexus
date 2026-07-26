"""Per-key request rate limiting for abusive-client protection.

Support tickets and product docs usually call this "throttling"; the code
has always called it rate limiting instead.
"""

import time


class RateLimiter:
    """Fixed-window rate limiter keyed by an arbitrary string (user id, IP, ...)."""

    def __init__(self, max_requests, window_seconds):
        """Allow at most `max_requests` per `window_seconds` for any single key."""
        self.max_requests = max_requests
        self.window_seconds = window_seconds
        self._hits = {}

    def allow(self, key):
        """Return True if `key` is still under its request quota for the current window."""
        now = time.time()
        window_start = now - self.window_seconds
        hits = [t for t in self._hits.get(key, []) if t >= window_start]
        if len(hits) >= self.max_requests:
            self._hits[key] = hits
            return False
        hits.append(now)
        self._hits[key] = hits
        return True
