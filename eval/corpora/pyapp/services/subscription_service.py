"""Recurring subscription lifecycle: renewal and cancellation."""

from services.billing_service import charge_card
from services.notification_service import send_email
from utils.time import utcnow

_SUBSCRIPTIONS = {}


def renew_subscription(subscription_id):
    """Charge the next billing cycle for `subscription_id` and push out its renewal date."""
    sub = _SUBSCRIPTIONS.get(subscription_id)
    if sub is None:
        raise ValueError(f"no such subscription: {subscription_id}")
    charge_card(sub.customer_id, sub.amount_cents)
    sub.renewed_at = utcnow()
    send_email(
        sub.customer_email,
        "Subscription renewed",
        "Your subscription has been renewed.",
    )
    return sub


def cancel_subscription(subscription_id):
    """Mark a subscription as cancelled so it is skipped on the next renewal sweep."""
    sub = _SUBSCRIPTIONS.get(subscription_id)
    if sub is None:
        raise ValueError(f"no such subscription: {subscription_id}")
    sub.status = "cancelled"
    return sub
