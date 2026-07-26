"""Scheduled job: release resources held by stale, abandoned sessions."""


def close(session_id):
    """Close a single stale session, releasing its server-side resources.

    Note: `services.billing_service` also has a `close` (for billing
    periods, not sessions) — same name, different job.
    """
    return {"session_id": session_id, "status": "closed"}


def cleanup_stale_sessions(session_ids):
    """Close every session in `session_ids` (already identified as stale by the caller)."""
    return [close(session_id) for session_id in session_ids]
