"""The `DunningStage` record: how far collection of an overdue account has gone."""

from enum import Enum


class DunningStage(Enum):
    """Escalation steps an overdue account moves through, mildest first."""

    COURTESY_NUDGE = "courtesy_nudge"
    FINAL_NOTICE = "final_notice"
    COLLECTIONS_HANDOFF = "collections_handoff"
    WRITTEN_OFF = "written_off"
