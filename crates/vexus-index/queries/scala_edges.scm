(call_expression
  function: (identifier) @call.name
  arguments: (arguments) @call.args) @call

(call_expression
  function: (field_expression field: (identifier) @call.name)
  arguments: (arguments) @call.args) @call

; Each `path:` element is captured in the same match; parse.rs keeps the
; last one, i.e. the imported symbol itself.
(import_declaration
  path: (identifier) @import.module)
