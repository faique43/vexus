; Both namespace forms: block-scoped and C# 10 file-scoped.
(namespace_declaration
  name: (_) @def.name) @def.module

(file_scoped_namespace_declaration
  name: (_) @def.name) @def.module

(class_declaration
  name: (identifier) @def.name) @def.class

(interface_declaration
  name: (identifier) @def.name) @def.interface

(enum_declaration
  name: (identifier) @def.name) @def.enum

(struct_declaration
  name: (identifier) @def.name) @def.struct

; Records are value classes; Class keeps method promotion working.
(record_declaration
  name: (identifier) @def.name) @def.class

; Methods/constructors promote to Method via the class-like parent.
; Properties are deliberately not symbols in v1: on small graphs they
; inflate noise for little lookup value.
(method_declaration
  name: (identifier) @def.name
  parameters: (parameter_list) @def.params) @def.function

(constructor_declaration
  name: (identifier) @def.name
  parameters: (parameter_list) @def.params) @def.function
