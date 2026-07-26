"""The `Subscription` record: a recurring billing arrangement."""

from dataclasses import dataclass
from typing import Optional


@dataclass
class Subscription:
    """A recurring billing arrangement tied to a customer's card on file."""

    id: str
    customer_id: str
    customer_email: str
    amount_cents: int
    status: str
    renewed_at: Optional[str] = None
