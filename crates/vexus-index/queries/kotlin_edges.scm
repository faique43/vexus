(call_expression
  (identifier) @call.name
  (value_arguments) @call.args) @call

; `obj.method(...)`: the callee is the navigation_expression's last child.
(call_expression
  (navigation_expression
    (identifier)
    (identifier) @call.name)
  (value_arguments) @call.args) @call

(import
  (qualified_identifier) @import.module)
