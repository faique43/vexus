"""Customer notification delivery, across channels."""


class EmailNotifier:
    """Sends notifications over email."""

    def send(self, message):
        """Deliver `message` over email (stands in for a real SMTP client call)."""
        return {"channel": "email", "message": message}


class SmsNotifier:
    """Sends notifications over SMS."""

    def send(self, message):
        """Deliver `message` over SMS (stands in for a real carrier gateway call)."""
        return {"channel": "sms", "message": message}


def notify_invoice_created(invoice, notifier):
    """Notify the customer that `invoice` was created, via whichever `notifier` was configured.

    Note: `notifier` is duck-typed — callers pass an `EmailNotifier`, an
    `SmsNotifier`, or any future channel exposing `send(self, message)` —
    so which one actually runs is decided entirely by the caller, not by
    this function.
    """
    return notifier.send(f"Invoice {invoice.id} for {invoice.amount_cents} created")


def send_email(to, subject, body):
    """Send a transactional email directly, bypassing the `Notifier` abstraction."""
    return EmailNotifier().send(f"{subject}: {body}")
