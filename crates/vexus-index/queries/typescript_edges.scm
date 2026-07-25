(call_expression
  function: (identifier) @call.name
  arguments: (arguments) @call.args) @call

(call_expression
  function: (member_expression property: (property_identifier) @call.name)
  arguments: (arguments) @call.args) @call

(import_statement
  source: (string (string_fragment) @import.module))
