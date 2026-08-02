; kotlin-ng represents classes, interfaces, enum classes and data classes
; all as class_declaration — Class for all of them keeps method promotion
; working, which is what matters for lookups.
(class_declaration
  name: (identifier) @def.name) @def.class

; Objects (and companion objects) are singleton values with members —
; Class keeps their functions promoting to methods.
(object_declaration
  name: (identifier) @def.name) @def.class

; function_value_parameters is a plain child, not a named field.
(function_declaration
  name: (identifier) @def.name
  (function_value_parameters) @def.params) @def.function

; Top-level `const val` only — a bare property_declaration would also match
; every local `val` inside function bodies.
(property_declaration
  (modifiers (property_modifier))
  (variable_declaration (identifier) @def.name)) @def.const
