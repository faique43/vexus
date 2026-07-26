"""The `Invoice` record: a single bill issued to a customer."""

from dataclasses import dataclass


@dataclass
class Invoice:
    """A bill issued to a customer for a given amount, in a given currency."""

    id: str
    customer_id: str
    amount_cents: int
    currency: str
    status: str
