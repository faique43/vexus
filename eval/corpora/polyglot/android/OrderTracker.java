package shop.android;

// In-app order status tracking shown on the order detail screen.
public class OrderTracker {
    private final String orderId;
    private int lastSeenStage;

    public OrderTracker(String orderId) {
        this.orderId = orderId;
        this.lastSeenStage = 0;
    }

    // Maps a raw webhook stage number onto the label the UI shows.
    public String stageLabel(int stage) {
        switch (stage) {
            case 0: return "placed";
            case 1: return "packed";
            case 2: return "shipped";
            default: return "delivered";
        }
    }

    // Advances the tracker and returns the label for the newest stage.
    public String advanceTo(int stage) {
        if (stage > lastSeenStage) {
            lastSeenStage = stage;
        }
        return stageLabel(lastSeenStage);
    }
}
