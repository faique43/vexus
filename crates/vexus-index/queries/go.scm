(function_declaration
  name: (identifier) @def.name
  parameters: (parameter_list) @def.params) @def.function

; Methods live at the top level in Go — the enclosing-parent promotion can
; never fire, so methodness is declared here. Capturing the `parameters:`
; field (not the `receiver:` field) keeps the receiver out of the arity by
; construction (CONTRIBUTING's value-params-only rule).
(method_declaration
  name: (field_identifier) @def.name
  parameters: (parameter_list) @def.params) @def.method

(type_declaration
  (type_spec
    name: (type_identifier) @def.name
    type: (struct_type))) @def.struct

(type_declaration
  (type_spec
    name: (type_identifier) @def.name
    type: (interface_type))) @def.interface

; Named type definitions over an existing type (`type Celsius float64`).
; struct/interface bodies are matched above; these two patterns don't
; overlap with them because the `type:` node types differ.
(type_declaration
  (type_spec
    name: (type_identifier) @def.name
    type: (type_identifier))) @def.type

(type_declaration
  (type_alias
    name: (type_identifier) @def.name)) @def.type

(const_declaration
  (const_spec name: (identifier) @def.name)) @def.const
