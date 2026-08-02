using System;
using System.Collections.Generic;

namespace Shop.Billing;

public class Invoice {
    private readonly List<int> lines;

    public Invoice(List<int> lines) {
        this.lines = lines;
    }

    // Total in cents, tax included.
    public int Total(int taxCents) {
        return Subtotal() + taxCents;
    }

    static int Subtotal() => 41;
}

public interface IRenderable {
    string Render();
}

public enum Status {
    Open,
    Paid
}

public record LineItem(int Qty, int UnitCents);

public struct Pair {
    public int A;
    public int B;
}
