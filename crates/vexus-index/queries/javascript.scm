(function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(method_definition
  name: (property_identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

; JS class names are plain identifiers (unlike TS's type_identifier).
(class_declaration
  name: (identifier) @def.name) @def.class

(generator_function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

; Const-assigned function forms — same three distinct node types as the
; TypeScript queries (see typescript.scm for the history).
(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (arrow_function parameters: (formal_parameters) @def.params))) @def.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (function_expression parameters: (formal_parameters) @def.params))) @def.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (generator_function parameters: (formal_parameters) @def.params))) @def.function

; Assignment-defined functions. `app.use = function use(fn) {}`,
; `Foo.prototype.bar = function () {}` and `exports.parse = (s) => {}` are
; the dominant CommonJS/prototype way to define a module's public API, and a
; query that only reads declarations and `const` sees none of them — Express
; indexed its entire `app.*` surface as two symbols before this. The
; property is the symbol name, and the resolver's `.` suffix matching ties
; `app.use(...)` call sites back to it the same way it already does for
; Lua's `M.helper`.
(assignment_expression
  left: (member_expression property: (property_identifier) @def.name)
  right: (function_expression parameters: (formal_parameters) @def.params)) @def.function

(assignment_expression
  left: (member_expression property: (property_identifier) @def.name)
  right: (arrow_function parameters: (formal_parameters) @def.params)) @def.function

(assignment_expression
  left: (member_expression property: (property_identifier) @def.name)
  right: (generator_function parameters: (formal_parameters) @def.params)) @def.function

; `var f = function () {}` — `var` parses as `variable_declaration`, a
; different node from the `lexical_declaration` `const`/`let` produce.
(variable_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (function_expression parameters: (formal_parameters) @def.params))) @def.function

(variable_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (arrow_function parameters: (formal_parameters) @def.params))) @def.function

(variable_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (generator_function parameters: (formal_parameters) @def.params))) @def.function

; Object-literal function values — `module.exports = { parse: function () {} }`.
(pair
  key: (property_identifier) @def.name
  value: (function_expression parameters: (formal_parameters) @def.params)) @def.function

(pair
  key: (property_identifier) @def.name
  value: (arrow_function parameters: (formal_parameters) @def.params)) @def.function

(pair
  key: (property_identifier) @def.name
  value: (generator_function parameters: (formal_parameters) @def.params)) @def.function
