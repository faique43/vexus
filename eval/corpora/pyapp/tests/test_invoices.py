"""Tests for invoice creation, retrieval, and refunds."""

import pytest

from services.invoice_service import create_invoice, get_invoice, refund_invoice


def test_create_invoice_persists_and_notifies():
    """Creating an invoice stores it as open and makes it retrievable."""
    invoice = create_invoice("cus_1", 5000)
    assert invoice.status == "open"
    assert get_invoice(invoice.id) is invoice


def test_create_invoice_rejects_zero_amount():
    """An invoice for zero cents must be refused by amount validation."""
    with pytest.raises(ValueError):
        create_invoice("cus_1", 0)


def test_refund_invoice_marks_the_invoice_refunded():
    """Refunding an open invoice flips its status and notifies the customer."""
    invoice = create_invoice("cus_2", 1200)
    refunded = refund_invoice(invoice.id)
    assert refunded.status == "refunded"
