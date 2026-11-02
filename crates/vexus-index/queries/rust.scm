(function_item
  name: (identifier) @def.name
  parameters: (parameters) @def.params) @def.function

(struct_item name: (type_identifier) @def.name) @def.struct
(enum_item name: (type_identifier) @def.name) @def.enum
(trait_item name: (type_identifier) @def.name) @def.trait
(const_item name: (identifier) @def.name) @def.const
(type_item name: (type_identifier) @def.name) @def.type

(function_signature_item
  name: (identifier) @def.name
  parameters: (parameters) @def.params) @def.function

; `impl` blocks scope what's inside them to a type without being symbols
; themselves: a type can have any number of them.
(impl_item type: (type_identifier) @def.name) @def.impl
(impl_item type: (generic_type type: (type_identifier) @def.name)) @def.impl
(impl_item type: (scoped_type_identifier name: (type_identifier) @def.name)) @def.impl
(impl_item type: (generic_type type: (scoped_type_identifier name: (type_identifier) @def.name))) @def.impl
(impl_item type: (reference_type type: (type_identifier) @def.name)) @def.impl

; Only inline modules. `mod foo;` has no body and lives in another file.
(mod_item name: (identifier) @def.name body: (declaration_list)) @def.module
