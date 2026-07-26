"""Opaque id generation for new records."""

import uuid


def generate_id(prefix):
    """Generate a new opaque id of the form `{prefix}_{hex}`."""
    return f"{prefix}_{uuid.uuid4().hex[:12]}"
