"""Retry helpers for calls to flaky external services (payment processors, carriers)."""

import random
import time


def retry_with_backoff(fn, attempts=3, base_delay=0.1):
    """Call `fn` up to `attempts` times, waiting an exponentially growing,
    jittered delay between failures so a burst of retries doesn't itself
    become a thundering herd against the downstream service.
    """
    last_exc = None
    for attempt in range(attempts):
        try:
            return fn()
        except Exception as exc:  # noqa: BLE001 - deliberately broad, generic retry wrapper
            last_exc = exc
            delay = base_delay * (2**attempt) + random.uniform(0, base_delay)
            time.sleep(delay)
    raise last_exc
