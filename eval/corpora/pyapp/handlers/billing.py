"""HTTP handlers for billing-cycle endpoints."""

from services import billing_service


class BillingHandler:
    """Routes billing HTTP requests to the billing service layer."""

    def charge(self, req):
        """Handle POST /billing/charge: charge a customer's card on file."""
        return billing_service.charge_card(req["customer_id"], req["amount_cents"])

    def close(self, req):
        """Handle POST /billing/close: close out the current billing period.

        Retrieval-challenge note: `services.billing_service` also defines a
        function named `close` (a different, module-level `close`); see
        `eval/edges/pyapp.yaml` for the same-name-different-module case this
        call site produces.
        """
        return billing_service.close(req["period_id"])
