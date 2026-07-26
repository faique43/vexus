"""HTTP handlers for authentication endpoints."""

from services.auth_service import authenticate, issue_token, revoke_token


class AuthHandler:
    """Routes authentication HTTP requests to the auth service layer."""

    def login(self, req):
        """Handle POST /auth/login: verify credentials and issue a session token."""
        user = authenticate(req["email"], req["password"])
        return issue_token(user)

    def logout(self, req):
        """Handle POST /auth/logout: invalidate the caller's session token."""
        return revoke_token(req["token"])

    def refresh_token(self, req):
        """Handle POST /auth/refresh: exchange a still-valid token for a new one."""
        return issue_token(req["user"])
