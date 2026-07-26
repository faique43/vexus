"""Scheduled job: renew every subscription that comes due today."""

from services.subscription_service import cancel_subscription, renew_subscription


def run_subscription_renewals(subscription_ids):
    """Attempt to renew each subscription, cancelling any whose charge fails."""
    results = []
    for subscription_id in subscription_ids:
        try:
            renew_subscription(subscription_id)
            results.append((subscription_id, "renewed"))
        except Exception:  # noqa: BLE001 - a failed charge should cancel, not crash the sweep
            cancel_subscription(subscription_id)
            results.append((subscription_id, "cancelled"))
    return results
