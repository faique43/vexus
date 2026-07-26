"""HTTP handlers for usage-report endpoints."""

from services.invoice_service import list_invoices
from services.report_service import build_usage_report


class ReportHandler:
    """Routes report HTTP requests to the report service layer."""

    def generate_usage_report(self, req):
        """Handle GET /reports/usage: build a usage report for the caller's customer."""
        invoices = list_invoices(req["customer_id"], req.get("page", 1))
        return build_usage_report(req["customer_id"], invoices)
