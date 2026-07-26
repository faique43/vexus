"""The `UsageReport` record: a point-in-time billing summary."""

from dataclasses import dataclass


@dataclass
class UsageReport:
    """A summary of a customer's invoice activity over their lifetime to date."""

    customer_id: str
    total_cents: int
    invoice_count: int
    generated_at: str
