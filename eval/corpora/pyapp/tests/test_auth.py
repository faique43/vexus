"""Tests for login throttling, token issuance, and revocation."""

import pytest

from services.auth_service import authenticate, issue_token, revoke_token


def test_authenticate_rejects_unknown_credentials():
    """A login with an email no user has must raise invalid credentials."""
    with pytest.raises(ValueError):
        authenticate("nobody@example.com", "hunter2")


def test_authenticate_throttles_repeated_login_attempts():
    """Hammering login for one email trips the rate limiter guard."""
    for _ in range(6):
        try:
            authenticate("brute@example.com", "wrong")
        except ValueError:
            pass
    with pytest.raises(ValueError, match="too many login attempts"):
        authenticate("brute@example.com", "wrong")


def test_issue_and_revoke_token_round_trip():
    """A freshly issued session token revokes exactly once."""

    class FakeUser:
        id = "usr_1"

    token = issue_token(FakeUser())
    assert revoke_token(token) is True
    assert revoke_token(token) is False
