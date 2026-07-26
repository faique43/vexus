"""Aggregate usage reports for billing and analytics."""

from models.report import UsageReport
from utils.time import utcnow


def build_usage_report(customer_id, invoices):
    """Summarize a customer's invoice activity to date into a `UsageReport`."""
    total_cents = sum(inv.amount_cents for inv in invoices if inv.status != "refunded")
    return UsageReport(
        customer_id=customer_id,
        total_cents=total_cents,
        invoice_count=len(invoices),
        generated_at=utcnow(),
    )
