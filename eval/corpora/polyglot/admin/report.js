// Admin daily-sales report rendering (plain JS, no build step).

// One currency cell, right-aligned to `width` characters.
function currencyCell(cents, width) {
  const text = `$${(cents / 100).toFixed(2)}`;
  return text.padStart(width);
}

// Renders the daily sales report as fixed-width text for the admin email.
const renderDailyReport = (rows) => {
  let body = "sku        revenue\n";
  for (const row of rows) {
    body += `${row.sku.padEnd(10)} ${currencyCell(row.revenueCents, 8)}\n`;
  }
  return body;
};

export { renderDailyReport };
