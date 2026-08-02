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

; Assigned function values — `local helper = function(...)`,
; `M.helper = function(...)`. Lua defines as many functions this way as it
; does with `function name(...)`, so a query that reads only the declaration
; form hides over half of a typical module (measured on plenary.nvim: 400
; declared, 427 assigned).
(assignment_statement
  (variable_list name: (identifier) @def.name)
  (expression_list value: (function_definition parameters: (parameters) @def.params))) @def.function

(assignment_statement
  (variable_list name: (dot_index_expression field: (identifier) @def.name))
  (expression_list value: (function_definition parameters: (parameters) @def.params))) @def.function

; Table-constructor fields — `local M = { helper = function(...) ... end }`.
(field
  name: (identifier) @def.name
  value: (function_definition parameters: (parameters) @def.params)) @def.function
