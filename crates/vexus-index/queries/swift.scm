; The Swift grammar folds classes, structs, enums and actors into
; class_declaration; protocols are separate. Extensions are also
; class_declaration but with a user_type name — capturing the inner
; type_identifier makes extension members nest under the extended type's
; name (unlike Rust impls, the name is right there in the node).
(class_declaration
  name: (type_identifier) @def.name) @def.class

(class_declaration
  name: (user_type (type_identifier) @def.name)) @def.class

(protocol_declaration
  name: (type_identifier) @def.name) @def.interface

; Parameters are individual `parameter` children with no wrapper node, so
; there's nothing to capture as @def.params — Swift symbols carry no arity
; (documented limitation; name-only edge resolution still applies).
(function_declaration
  name: (simple_identifier) @def.name) @def.function

(protocol_function_declaration
  name: (simple_identifier) @def.name) @def.function
