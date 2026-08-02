; `receiver.method(args)`.
(member_access
  (selector (unconditional_assignable_selector (identifier) @call.name))
  .
  (selector (argument_part (arguments) @call.args))) @call

; Bare invocation `helper(args)` — an identifier directly followed by an
; argument selector.
(member_access
  (identifier) @call.name
  .
  (selector (argument_part (arguments) @call.args))) @call

; The uri text keeps its quotes, like Go's interpreted_string_literal.
(import_specification
  (configurable_uri (uri (string_literal) @import.module)))
