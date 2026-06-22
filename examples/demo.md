# Tech Reader demo

This file exercises all three reading altitudes. Open it and run **Tech Reader: Read File**
(or click the speaker icon in the editor title bar).

## Prose

This paragraph is ordinary documentation. It should be read naturally and faithfully —
identifiers like `max_retries` and `getUserById` are spoken as words, but no information is
dropped.

## Code

The function below should be *explained*, not read character by character. You should hear
something like: "Hello function accepts a single parameter, name, which is expected to be a
string. If name is not empty, it returns a personalized greeting. Otherwise, it returns Hello,
World."

```typescript
function hello(name: string): string {
  if (name) {
    return `Hello, ${name}`;
  }
  return "Hello, World!";
}
```

## A table

The table below should be *distilled* to its takeaway, not read cell by cell.

| Attribute | PMI (bu 27, on WAVE_NEXT) | PACT (bu 28, on LEGACY) |
| --- | --- | --- |
| `order_source` | `:SAP` | `:SHOPIFY` |
| `order_type` | customer order | `:RESALE` |
| `does_resale?` | false | **true** |
| `has_item_master?` | true | **false — but item master data exists** |
| Inventory model | item-master + lot/batch allocation | resale units, picked by tag |
| Outbound messages | I06/I07/I05 SAP messages | Shopify fulfillment tracking only |
