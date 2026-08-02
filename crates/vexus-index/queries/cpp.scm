(namespace_definition
  name: (namespace_identifier) @def.name) @def.module

; Free functions and (via parent-kind promotion) in-class methods — the
; declarator is an identifier at file/namespace scope and a field_identifier
; inside a class body, so both need a pattern. template_declaration wrappers
; don't hide these: the query matches the inner function_definition directly.
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

; Out-of-class definitions (`int shop::Cart::total(...)`): the whole
; qualified_identifier is the name, so the symbol keeps its `::` path and
; the resolver's `::` suffix matching finds it.
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

; Prototypes, like C's.
(declaration
  declarator: (function_declarator
    declarator: (identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

(class_specifier
  name: (type_identifier) @def.name
  body: (field_declaration_list)) @def.class

(struct_specifier
  name: (type_identifier) @def.name
  body: (field_declaration_list)) @def.struct

(enum_specifier
  name: (type_identifier) @def.name
  body: (enumerator_list)) @def.enum

(type_definition
  declarator: (type_identifier) @def.name) @def.type

(alias_declaration
  name: (type_identifier) @def.name) @def.type
