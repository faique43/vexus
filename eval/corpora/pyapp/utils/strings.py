"""String normalization helpers for user-facing text."""

import re


def normalize_name(name):
    """Collapse repeated whitespace and title-case a user-supplied display name."""
    collapsed = re.sub(r"\s+", " ", name.strip())
    return collapsed.title()


def slugify(text):
    """Turn arbitrary text into a lowercase, hyphen-separated URL slug."""
    lowered = text.strip().lower()
    return re.sub(r"[^a-z0-9]+", "-", lowered).strip("-")
