// Receipt formatting for the native point-of-sale terminal.
#include <string>

namespace pos {

// Right-aligns a cent amount as dollars into a fixed-width column.
std::string money_cell(int cents, int width) {
  std::string text = "$" + std::to_string(cents / 100) + "." + std::to_string(cents % 100);
  while ((int)text.size() < width) text = " " + text;
  return text;
}

// Renders the printed receipt body for one sale.
std::string render_receipt(int subtotal_cents, int tax_cents) {
  std::string body = "subtotal" + money_cell(subtotal_cents, 10) + "\n";
  body += "tax" + money_cell(tax_cents, 15) + "\n";
  body += "total" + money_cell(subtotal_cents + tax_cents, 13) + "\n";
  return body;
}

}  // namespace pos
