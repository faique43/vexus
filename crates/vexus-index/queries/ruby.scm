(module
  name: (constant) @def.name) @def.module

(class
  name: (constant) @def.name) @def.class

; parameters are optional in Ruby (`def subtotal`) — the `?` keeps
; zero-param methods captured, with arity None.
(method
  name: (identifier) @def.name
  parameters: (method_parameters)? @def.params) @def.function

; `def self.x` — methodness is syntactic, not parent-derived.
(singleton_method
  name: (identifier) @def.name
  parameters: (method_parameters)? @def.params) @def.method
