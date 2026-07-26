"""Application entrypoint: wires HTTP routes to their handlers."""

from handlers.auth import AuthHandler
from handlers.billing import BillingHandler
from handlers.invoices import InvoiceHandler
from handlers.reports import ReportHandler
from handlers.users import UserHandler
from handlers.webhooks import WebhookHandler


def build_routes():
    """Return the route table mapping each HTTP path to its handler method."""
    invoices = InvoiceHandler()
    users = UserHandler()
    auth = AuthHandler()
    billing = BillingHandler()
    webhooks = WebhookHandler()
    reports = ReportHandler()
    return {
        "POST /invoices": invoices.create,
        "GET /invoices/:id": invoices.get,
        "GET /invoices": invoices.list,
        "POST /invoices/:id/refund": invoices.refund,
        "POST /users": users.create,
        "GET /users/:id": users.get,
        "PATCH /users/:id/profile": users.update_profile,
        "POST /auth/login": auth.login,
        "POST /auth/logout": auth.logout,
        "POST /auth/refresh": auth.refresh_token,
        "POST /billing/charge": billing.charge,
        "POST /billing/close": billing.close,
        "POST /webhooks/stripe": webhooks.receive_stripe_event,
        "GET /reports/usage": reports.generate_usage_report,
    }


def dispatch_create(handler, req):
    """Invoke `handler`'s `create` endpoint directly, bypassing the route
    table. Used by internal admin tooling that already knows which handler
    it wants and only needs the plain-Python calling convention.

    Deliberately duck-typed: any handler exposing a `create(self, req)`
    method works here, so a static call-graph can't know which concrete
    handler `handler` will be at a given call site.
    """
    return handler.create(req)


def main():
    """Boot the service by wiring the route table and printing a summary."""
    routes = build_routes()
    print(f"serving {len(routes)} routes")


if __name__ == "__main__":
    main()
