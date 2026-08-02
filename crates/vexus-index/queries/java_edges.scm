(method_invocation
  name: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(object_creation_expression
  type: (type_identifier) @call.name
  arguments: (argument_list) @call.args) @call

(import_declaration
  (scoped_identifier) @import.module)
