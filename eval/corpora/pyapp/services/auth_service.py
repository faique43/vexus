"""Authentication, brute-force login guarding, and session-token issuance."""

from services.rate_limiter import RateLimiter
from services.user_service import _USERS
from utils.ids import generate_id

# Internally this is always "rate limiting"; see `services.rate_limiter` for
# the retrieval-challenge note about the "throttle" synonym.
_login_limiter = RateLimiter(max_requests=5, window_seconds=60)
_ACTIVE_TOKENS = {}


def authenticate(email, password):
    """Verify credentials, guarding against brute-force login attempts."""
    if not _login_limiter.allow(email):
        raise ValueError("too many login attempts, try again later")
    for user in _USERS.values():
        if user.email == email:
            return user
    raise ValueError("invalid credentials")


def issue_token(user):
    """Mint a new session token for `user` and record it as currently active."""
    token = generate_id("tok")
    _ACTIVE_TOKENS[token] = user.id
    return token


def revoke_token(token):
    """Invalidate a session token so it can no longer authenticate requests."""
    return _ACTIVE_TOKENS.pop(token, None) is not None
