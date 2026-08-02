package shop;

import java.util.List;

public class Order {
    private final List<String> lines;

    public Order(List<String> lines) {
        this.lines = lines;
    }

    // Total line count.
    public int lineCount() {
        return lines.size();
    }

    public String summary(boolean verbose) {
        var count = lineCount();
        return verbose ? new Description(count).render() : String.valueOf(count);
    }
}

interface Renderable {
    String render();
}

enum Status {
    OPEN,
    SHIPPED
}

record Description(int count) implements Renderable {
    public String render() {
        return "order with " + count + " lines";
    }
}
