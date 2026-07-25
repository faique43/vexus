(call_expression
  function: (identifier) @call.name
  arguments: (arguments) @call.args) @call

(call_expression
  function: (field_expression field: (field_identifier) @call.name)
  arguments: (arguments) @call.args) @call

(call_expression
  function: (scoped_identifier name: (identifier) @call.name)
  arguments: (arguments) @call.args) @call

(use_declaration argument: (scoped_identifier) @import.module)
(use_declaration argument: (identifier) @import.module)
