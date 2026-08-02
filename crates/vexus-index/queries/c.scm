; The function name hides inside the declarator chain, so the pattern
; reaches through function_declarator to the inner identifier.
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

; Prototypes (headers) — same shape under a plain declaration, like Rust's
; function_signature_item.
(declaration
  declarator: (function_declarator
    declarator: (identifier) @def.name
    parameters: (parameter_list) @def.params)) @def.function

; `body:` restricts to real definitions — `struct Foo;` forward
; declarations and `struct Foo x;` uses have no body and stay out.
(struct_specifier
  name: (type_identifier) @def.name
  body: (field_declaration_list)) @def.struct

(enum_specifier
  name: (type_identifier) @def.name
  body: (enumerator_list)) @def.enum

(type_definition
  declarator: (type_identifier) @def.name) @def.type

; #define'd constants/macros are preproc nodes with different structure —
; documented limitation, not captured in v1.
