(call_expression
  (simple_identifier) @call.name
  (call_suffix (value_arguments) @call.args)) @call

; `target.method(...)`: the callee is the navigation suffix.
(call_expression
  (navigation_expression
    suffix: (navigation_suffix suffix: (simple_identifier) @call.name))
  (call_suffix (value_arguments) @call.args)) @call

(import_declaration
  (identifier) @import.module)
