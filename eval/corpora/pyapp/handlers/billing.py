"""HTTP handlers for billing-cycle endpoints."""

from services import billing_service


class BillingHandler:
    """Routes billing HTTP requests to the billing service layer."""

    def charge(self, req):
        """Handle POST /billing/charge: charge a customer's card on file."""
        return billing_service.charge_card(req["customer_id"], req["amount_cents"])

    def close(self, req):
        """Handle POST /billing/close: close out the current billing period.

        Note: delegates to services.billing_service.close, a separate
        module-level function that happens to share this method's name.
        """
        return billing_service.close(req["period_id"])
