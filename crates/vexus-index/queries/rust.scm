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
