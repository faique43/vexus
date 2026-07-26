"""Card charging and billing-cycle close-out logic."""

from utils.retry import retry_with_backoff


def charge_card(customer_id, amount_cents):
    """Charge the customer's card on file for `amount_cents`, retrying transient failures."""
    return retry_with_backoff(lambda: _submit_charge(customer_id, amount_cents))


def close(period_id):
    """Close out the billing period identified by `period_id`, locking it
    against further charges.

    Retrieval-challenge note: `jobs.cleanup` also defines a function named
    `close` (closing a stale session, not a billing period) — see
    `eval/edges/pyapp.yaml` for the same-name-different-module case this
    produces for any caller that reaches this through an unqualified name.
    """
    return {"period_id": period_id, "status": "closed"}


def _submit_charge(customer_id, amount_cents):
    """Low-level card charge submission (stands in for a real payment processor call)."""
    return {"customer_id": customer_id, "amount_cents": amount_cents, "status": "charged"}
