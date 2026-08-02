(function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(method_definition
  name: (property_identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

; JS class names are plain identifiers (unlike TS's type_identifier).
(class_declaration
  name: (identifier) @def.name) @def.class

(generator_function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

; Const-assigned function forms — same three distinct node types as the
; TypeScript queries (see typescript.scm for the history).
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
