; tree-sitter-dart 0.0.4 has some idiosyncratic shapes: top-level
; functions parse as lambda_expression, and mixins carry no name field.

(class_definition
  name: (identifier) @def.name) @def.class

(mixin_declaration
  (identifier) @def.name) @def.trait

(enum_declaration
  name: (identifier) @def.name) @def.enum

; Extensions nest their members under the extension's own name.
(extension_declaration
  name: (identifier) @def.name) @def.class

; Methods: class_member_definition wraps signature + body, so it (not the
; signature) is the def node and line ranges cover the whole body.
(class_member_definition
  (method_signature
    (function_signature
      name: (identifier) @def.name
      (formal_parameter_list) @def.params))) @def.function

(class_member_definition
  (declaration
    (constructor_signature
      name: (identifier) @def.name
      parameters: (formal_parameter_list) @def.params))) @def.function

; Top-level functions.
(lambda_expression
  parameters: (function_signature
    name: (identifier) @def.name
    (formal_parameter_list) @def.params)
  body: (function_body)) @def.function
