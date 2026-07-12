// Data-driven invoice fixture for the Typst backend.
// The render service injects the request's JSON as `sys.inputs.data` (a string);
// we parse it back into a Typst value here.
#let data = json(bytes(sys.inputs.data))

#set page(paper: "a4", margin: 2cm)
#set text(size: 11pt)

= Invoice #data.number

*Billed to:* #data.customer

#v(0.5em)

#table(
  columns: (1fr, auto, auto, auto),
  align: (left, right, right, right),
  table.header([*Item*], [*Qty*], [*Unit*], [*Amount*]),
  ..data.items.map(item => (
    [#item.name],
    [#item.qty],
    [\$#item.unit],
    [\$#(item.qty * item.unit)],
  )).flatten()
)

#v(1em)

#align(right)[
  #text(size: 13pt, weight: "bold")[Total: \$#data.total]
]
