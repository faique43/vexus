"""Timestamp helpers shared across services and jobs."""

from datetime import datetime, timezone


def utcnow():
    """Return the current UTC time as an ISO-8601 string."""
    return datetime.now(timezone.utc).isoformat()


def format_timestamp(dt):
    """Render a datetime as the human-readable format used in emails and reports."""
    return dt.strftime("%B %d, %Y at %H:%M UTC")
