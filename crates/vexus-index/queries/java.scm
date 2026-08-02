(class_declaration
  name: (identifier) @def.name) @def.class

(interface_declaration
  name: (identifier) @def.name) @def.interface

(enum_declaration
  name: (identifier) @def.name) @def.enum

; Records are value classes; Class keeps method promotion working.
(record_declaration
  name: (identifier) @def.name) @def.class

; Methods/constructors nest inside a class-like parent, so the shared
; parent-kind promotion turns these into Method automatically.
(method_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(constructor_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function
