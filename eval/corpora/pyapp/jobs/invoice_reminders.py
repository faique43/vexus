"""Scheduled job: remind customers about invoices approaching their due date."""

from services.invoice_service import list_invoices
from services.notification_service import send_email


def send_invoice_reminders(customer_ids):
    """Send a reminder email for every still-open invoice belonging to each customer."""
    sent = 0
    for customer_id in customer_ids:
        for invoice in list_invoices(customer_id):
            if invoice.status == "open":
                send_email(
                    customer_id, "Invoice due soon", f"Invoice {invoice.id} is due soon."
                )
                sent += 1
    return sent
