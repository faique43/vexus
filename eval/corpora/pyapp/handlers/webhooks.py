"""HTTP handlers for inbound third-party webhooks."""

from services.invoice_service import get_invoice
from services.notification_service import send_email


class WebhookHandler:
    """Routes inbound webhook payloads to the relevant internal service."""

    def receive_stripe_event(self, req):
        """Handle POST /webhooks/stripe: react to a Stripe payment-lifecycle event."""
        event_type = req["type"]
        if event_type == "charge.failed":
            return self._handle_charge_failed(req)
        return {"status": "ignored", "type": event_type}

    def _handle_charge_failed(self, req):
        """Notify the customer that their card was declined on a failed charge."""
        invoice = get_invoice(req["invoice_id"])
        if invoice is not None:
            send_email(invoice.customer_id, "Payment failed", "Your card was declined.")
        return {"status": "handled"}
