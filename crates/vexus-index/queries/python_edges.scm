(call
  function: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(call
  function: (attribute attribute: (identifier) @call.name)
  arguments: (argument_list) @call.args) @call

(import_statement
  name: (dotted_name) @import.module)

(import_from_statement
  module_name: (dotted_name) @import.module)
