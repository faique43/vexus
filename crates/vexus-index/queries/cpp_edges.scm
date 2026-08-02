(call_expression
  function: (identifier) @call.name
  arguments: (argument_list) @call.args) @call

(call_expression
  function: (field_expression field: (field_identifier) @call.name)
  arguments: (argument_list) @call.args) @call

; Qualified calls (`shop::helper(1)`): the innermost qualified_identifier's
; `name:` is the plain identifier.
(call_expression
  function: (qualified_identifier name: (identifier) @call.name)
  arguments: (argument_list) @call.args) @call

(preproc_include
  path: (string_literal) @import.module)

(preproc_include
  path: (system_lib_string) @import.module)
