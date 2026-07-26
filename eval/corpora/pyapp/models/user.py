"""The `User` record: a registered account holder."""

from dataclasses import dataclass


@dataclass
class User:
    """A registered account holder with a display name and contact email."""

    id: str
    email: str
    display_name: str
    bio: str
