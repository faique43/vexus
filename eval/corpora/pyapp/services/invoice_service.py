"""Business logic for creating, retrieving, listing, and refunding invoices."""

from models.invoice import Invoice
from services.billing_service import charge_card
from services.notification_service import EmailNotifier, notify_invoice_created
from utils.ids import generate_id
from utils.pagination import paginate
from utils.validation import validate_amount

_INVOICES = {}


def create_invoice(customer_id, amount_cents, currency="usd"):
    """Create and persist a new invoice, then notify the customer it exists."""
    validate_amount(amount_cents)
    invoice = Invoice(
        id=generate_id("inv"),
        customer_id=customer_id,
        amount_cents=amount_cents,
        currency=currency,
        status="open",
    )
    _INVOICES[invoice.id] = invoice
    notify_invoice_created(invoice, _default_notifier())
    return invoice


def get_invoice(invoice_id):
    """Fetch a single invoice by id, or None if it doesn't exist."""
    return _INVOICES.get(invoice_id)


def list_invoices(customer_id, page=1):
    """List invoices belonging to `customer_id`, one page at a time."""
    matches = [inv for inv in _INVOICES.values() if inv.customer_id == customer_id]
    return paginate(matches, page)


def refund_invoice(invoice_id, amount_cents=None):
    """Refund an invoice in full, or partially if `amount_cents` is given."""
    invoice = get_invoice(invoice_id)
    if invoice is None:
        raise ValueError(f"no such invoice: {invoice_id}")
    refund_amount = amount_cents or invoice.amount_cents
    charge_card(invoice.customer_id, -refund_amount)
    invoice.status = "refunded"
    return invoice


def _default_notifier():
    """Return the notifier used for invoice lifecycle events (email, by default)."""
    return EmailNotifier()
