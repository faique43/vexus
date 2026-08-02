package shop.analytics

// Checkout funnel drop-off calculations for the analytics job.
object Funnel {
  // Fraction of sessions that survived from one step to the next.
  def stepRate(entered: Int, completed: Int): Double = {
    if (entered == 0) 0.0 else completed.toDouble / entered
  }

  // Overall conversion through every step of the funnel.
  def conversion(steps: List[(Int, Int)]): Double = {
    steps.map { case (e, c) => stepRate(e, c) }.product
  }
}
