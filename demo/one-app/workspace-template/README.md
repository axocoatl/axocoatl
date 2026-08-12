# Northstar Supply orders

A deliberately small storefront fixture for the Axocoatl one-app demonstration.

The repository begins with one customer-visible regression: a fixed discount larger
than the subtotal produces a negative payable total. The tests state the contract.

```bash
npm run check  # intentionally red before the demo fix
npm run demo   # storefront preview on http://127.0.0.1:8765
```

The fixture has no package dependencies and does not need an install step.
