"""HTTP handlers for invoice endpoints."""

from services.invoice_service import (
    create_invoice,
    get_invoice,
    list_invoices,
    refund_invoice,
)


class InvoiceHandler:
    """Routes invoice HTTP requests to the invoice service layer."""

    def create(self, req):
        """Handle POST /invoices: create a new invoice for the customer."""
        return create_invoice(
            req["customer_id"], req["amount_cents"], req.get("currency", "usd")
        )

    def get(self, req):
        """Handle GET /invoices/:id: fetch a single invoice by id."""
        return get_invoice(req["id"])

    def list(self, req):
        """Handle GET /invoices: list invoices for the requesting customer."""
        return list_invoices(req["customer_id"], req.get("page", 1))

    def refund(self, req):
        """Handle POST /invoices/:id/refund: issue a full or partial refund."""
        return refund_invoice(req["id"], req.get("amount_cents"))
