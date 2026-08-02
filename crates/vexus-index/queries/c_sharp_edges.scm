(invocation_expression
  function: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(invocation_expression
  function: (member_access_expression name: (identifier) @call.name)
  arguments: (argument_list) @call.args) @call

(object_creation_expression
  type: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(using_directive
  (identifier) @import.module)

(using_directive
  (qualified_name) @import.module)
