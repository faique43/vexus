"""Input validation and sanitization shared across services."""

import re

_EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")


def sanitize_email(address):
    """Normalize an email address and reject one shaped to smuggle extra
    SMTP headers into outbound mail (a classic email header injection).
    """
    cleaned = address.strip().lower()
    if "\n" in cleaned or "\r" in cleaned:
        raise ValueError("invalid email address")
    if not _EMAIL_RE.match(cleaned):
        raise ValueError("invalid email address")
    return cleaned


def validate_amount(amount_cents):
    """Reject non-positive or absurdly large charge amounts before they reach the processor."""
    if amount_cents <= 0:
        raise ValueError("amount must be positive")
    if amount_cents > 10_000_000_00:
        raise ValueError("amount exceeds maximum allowed charge")
