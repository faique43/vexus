(function_call
  name: (identifier) @call.name
  arguments: (arguments) @call.args
  (#not-eq? @call.name "require")) @call

(function_call
  name: (dot_index_expression field: (identifier) @call.name)
  arguments: (arguments) @call.args) @call

(function_call
  name: (method_index_expression method: (identifier) @call.name)
  arguments: (arguments) @call.args) @call

(function_call
  name: (identifier) @_kw
  arguments: (arguments (string content: (string_content) @import.module))
  (#eq? @_kw "require"))
