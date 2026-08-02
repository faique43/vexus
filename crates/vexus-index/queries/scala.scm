; Scala objects are value-like singletons with members — Class keeps
; method promotion working.
(object_definition
  name: (identifier) @def.name) @def.class

(class_definition
  name: (identifier) @def.name) @def.class

(trait_definition
  name: (identifier) @def.name) @def.trait

(enum_definition
  name: (identifier) @def.name) @def.enum

(function_definition
  name: (identifier) @def.name
  parameters: (parameters) @def.params) @def.function

; Abstract members in traits (`def render(): String` with no body).
(function_declaration
  name: (identifier) @def.name
  parameters: (parameters) @def.params) @def.function
