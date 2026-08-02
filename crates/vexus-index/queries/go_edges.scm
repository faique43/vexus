(call_expression
  function: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(call_expression
  function: (selector_expression field: (field_identifier) @call.name)
  arguments: (argument_list) @call.args) @call

(import_spec
  path: (interpreted_string_literal) @import.module)
