(function_declaration
  name: (identifier) @def.name
  parameters: (parameters) @def.params) @def.function

; `function M.helper(...)` — the trailing identifier is the symbol name;
; the resolver's `.` suffix matching connects dotted call sites to it.
(function_declaration
  name: (dot_index_expression field: (identifier) @def.name)
  parameters: (parameters) @def.params) @def.function

; `function M:method(...)` — colon methods take an implicit self, which
; never appears in `parameters`, so arity stays receiver-free.
(function_declaration
  name: (method_index_expression method: (identifier) @def.name)
  parameters: (parameters) @def.params) @def.method
