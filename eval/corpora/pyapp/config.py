"""Runtime configuration loaded from environment variables."""

import os
from dataclasses import dataclass


@dataclass
class Config:
    """Immutable snapshot of environment-derived settings for this process."""

    database_url: str
    stripe_api_key: str
    max_login_attempts: int


def load_config():
    """Read configuration from the environment, applying sane defaults for
    local development so the app still boots without a `.env` file.
    """
    return Config(
        database_url=os.environ.get("DATABASE_URL", "sqlite:///app.db"),
        stripe_api_key=os.environ.get("STRIPE_API_KEY", ""),
        max_login_attempts=int(os.environ.get("MAX_LOGIN_ATTEMPTS", "5")),
    )
