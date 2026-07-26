"""Business logic for user account creation and profile management."""

from models.user import User
from utils.ids import generate_id
from utils.strings import normalize_name
from utils.validation import sanitize_email

_USERS = {}


def create_user(email, display_name):
    """Register a new user account with a normalized name and sanitized email."""
    user = User(
        id=generate_id("usr"),
        email=sanitize_email(email),
        display_name=normalize_name(display_name),
        bio="",
    )
    _USERS[user.id] = user
    return user


def get_user(user_id):
    """Fetch a user's record by id, or None if it doesn't exist."""
    return _USERS.get(user_id)


def update_profile(user_id, display_name=None, bio=None):
    """Apply partial updates to a user's mutable profile fields."""
    user = get_user(user_id)
    if user is None:
        raise ValueError(f"no such user: {user_id}")
    if display_name is not None:
        user.display_name = normalize_name(display_name)
    if bio is not None:
        user.bio = bio
    return user
