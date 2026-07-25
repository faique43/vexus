(function_declaration
  name: (identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(method_definition
  name: (property_identifier) @def.name
  parameters: (formal_parameters) @def.params) @def.function

(class_declaration
  name: (type_identifier) @def.name) @def.class

(interface_declaration
  name: (type_identifier) @def.name) @def.interface

(lexical_declaration
  (variable_declarator
    name: (identifier) @def.name
    value: (arrow_function parameters: (formal_parameters) @def.params))) @def.function
