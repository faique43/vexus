"""HTTP handlers for user account endpoints."""

from services.user_service import create_user, get_user, update_profile


class UserHandler:
    """Routes user HTTP requests to the user service layer."""

    def create(self, req):
        """Handle POST /users: register a new user account."""
        return create_user(req["email"], req["display_name"])

    def get(self, req):
        """Handle GET /users/:id: fetch a user's public profile."""
        return get_user(req["id"])

    def update_profile(self, req):
        """Handle PATCH /users/:id/profile: apply mutable profile field updates."""
        return update_profile(req["id"], req.get("display_name"), req.get("bio"))
