// A4 sheet cut to the Avery L7163 / 7163-compatible layout: 99.09 x 38.1 mm
// rectangle labels, 2 columns x 7 rows, 14 labels per sheet.
// https://www.averyproducts.com.au/blank-labels/rectangle-99x38mm
// Geometry (origin/pitch/gap) sourced from the glabels Avery L7163 template.
#set page(
  paper: "a4",
  margin: (top: 15.17mm, bottom: 15.13mm, left: 3.35mm, right: 4.54mm),
)
#set text(font: "New Computer Modern", size: 10pt)
#set par(justify: false, leading: 0.65em)

#let label-width = 99.09mm
#let label-height = 38.1mm
#let label-gap = 3.92mm
#let inner-margin = 1.76mm

// Renders one label cell. `cell` is a dict with either:
// - `(kind: "placeholder", text: str)` for a standalone message (e.g. "no
//   customers found"), rendered in italics with no address lines, or
// - `(kind: "customer", name: str, lines: array<str>, no_address: bool)`
//   for a customer's name (bold) followed by their address lines, or an
//   italic "No address on file" if `no_address` is true.
#let shipping-label(cell) = box(width: label-width, height: label-height)[
  #align(horizon)[
    #pad(left: inner-margin, right: inner-margin)[
      #if cell.kind == "placeholder" [
        #emph[#cell.text]
      ] else [
        #strong[#cell.name]
        #if cell.no_address [
          #linebreak()
          #emph[No address on file]
        ] else [
          #for line in cell.lines [
            #linebreak()
            #line
          ]
        ]
      ]
    ]
  ]
]

#import sys: inputs

// `inputs.pages` is an array of pages, each an array of up to 14 label
// cells (2 columns x 7 rows), pre-chunked by the caller.
#for (page-index, page) in inputs.pages.enumerate() [
  #if page-index > 0 [
    #pagebreak()
  ]
  #grid(
    columns: (label-width, label-width),
    column-gutter: label-gap,
    rows: label-height,
    ..page.map(shipping-label),
  )
]
