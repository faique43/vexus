; namespace_name's text keeps the `\` separators; the resolver's suffix
; matching understands them.
(namespace_definition
  name: (namespace_name) @def.name) @def.module

(function_definition
  name: (name) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(method_declaration
  name: (name) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(class_declaration
  name: (name) @def.name) @def.class

(interface_declaration
  name: (name) @def.name) @def.interface

(trait_declaration
  name: (name) @def.name) @def.trait

(enum_declaration
  name: (name) @def.name) @def.enum

(const_declaration
  (const_element (name) @def.name)) @def.const
