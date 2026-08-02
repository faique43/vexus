package shop.mobile

// Drives the mobile checkout screen's step progression.
class CheckoutFlow(private val steps: List<String>) {
    private var index = 0

    // Human label for the step the user is currently on.
    fun currentStep(): String = steps.getOrElse(index) { "done" }

    // Advances and returns the new step's label.
    fun advance(): String {
        if (index < steps.size) index += 1
        return currentStep()
    }
}

// Builds the standard three-step checkout used by the mobile app.
fun standardCheckout(): CheckoutFlow {
    return CheckoutFlow(listOf("address", "payment", "confirm"))
}
