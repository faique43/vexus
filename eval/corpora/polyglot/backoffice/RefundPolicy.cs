using System;

namespace Shop.Backoffice;

// Decides whether a refund request is auto-approved or needs review.
public class RefundPolicy {
    private readonly int autoApproveLimitCents;

    public RefundPolicy(int autoApproveLimitCents) {
        this.autoApproveLimitCents = autoApproveLimitCents;
    }

    // Refunds under the limit and within 30 days auto-approve.
    public bool AutoApproves(int amountCents, int ageDays) {
        return amountCents <= autoApproveLimitCents && WithinWindow(ageDays);
    }

    static bool WithinWindow(int ageDays) {
        return ageDays <= 30;
    }
}
