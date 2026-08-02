(function_call_expression
  function: (name) @call.name
  arguments: (arguments) @call.args) @call

; `\App\helper(...)` — the qualified name keeps its backslashes; suffix
; matching resolves it.
(function_call_expression
  function: (qualified_name) @call.name
  arguments: (arguments) @call.args) @call

(member_call_expression
  name: (name) @call.name
  arguments: (arguments) @call.args) @call

(scoped_call_expression
  name: (name) @call.name
  arguments: (arguments) @call.args) @call

(object_creation_expression
  (name) @call.name
  (arguments) @call.args) @call

(namespace_use_declaration
  (namespace_use_clause (qualified_name) @import.module))
