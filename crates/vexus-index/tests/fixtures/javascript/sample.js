import { formatPrice } from "./format";

// Renders one cart row.
function renderRow(item, index) {
  return `${index}: ${formatPrice(item.price)}`;
}

const sumTotals = (items) => {
  return items.reduce((acc, it) => acc + it.price, 0);
};

const legacyHelper = function (x) {
  return renderRow(x, 0);
};

function* idGenerator() {
  let i = 0;
  while (true) yield i++;
}

class Cart {
  constructor(items) {
    this.items = items;
  }

  // Total in cents.
  total() {
    return sumTotals(this.items);
  }
}

// CommonJS surface: assignment-defined methods must index like declarations.
const app = {};

app.use = function use(fn) {
  return renderRow(fn, 1);
};

app.render = (name) => sumTotals([name]);

Cart.prototype.clear = function () {
  this.items = [];
};

var legacyVar = function (n) {
  return n * 2;
};

module.exports = {
  buildCart: function (items) {
    return new Cart(items);
  },
  emptyCart: () => new Cart([]),
};
