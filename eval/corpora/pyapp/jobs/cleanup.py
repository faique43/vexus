"""Scheduled job: release resources held by stale, abandoned sessions."""


def close(session_id):
    """Close a single stale session, releasing its server-side resources.

    Retrieval-challenge note: `services.billing_service` also defines a
    function named `close` (closing a billing period, not a session) — see
    `eval/edges/pyapp.yaml` for the same-name-different-module case this
    produces for any caller that reaches either through an unqualified name.
    """
    return {"session_id": session_id, "status": "closed"}


def cleanup_stale_sessions(session_ids):
    """Close every session in `session_ids` (already identified as stale by the caller)."""
    return [close(session_id) for session_id in session_ids]
