(function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(method_definition
  name: (property_identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(class_declaration
  name: (type_identifier) @def.name) @def.class

(interface_declaration
  name: (type_identifier) @def.name) @def.interface

(generator_function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

; Const-assigned function forms. `arrow_function`, `function_expression` and
; `generator_function` are distinct node types, so each needs its own
; pattern — matching only the arrow (as this file originally did) silently
; drops `const f = function ...` / `const g = async function* ...` symbols,
; and with them every callers/callees/impact answer about those functions.
(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (arrow_function parameters: (formal_parameters) @def.params))) @def.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (function_expression parameters: (formal_parameters) @def.params))) @def.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (generator_function parameters: (formal_parameters) @def.params))) @def.function
