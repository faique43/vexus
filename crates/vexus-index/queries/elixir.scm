; Elixir has no function_definition node: def/defp/defmodule are ordinary
; (call) nodes whose target names them — the #eq?/#any-of? predicates
; (applied by the tree-sitter Rust binding) do the discrimination.

(call
  target: (identifier) @_kw
  (arguments (alias) @def.name)
  (#eq? @_kw "defmodule")) @def.module

(call
  target: (identifier) @_kw
  (arguments (call
    target: (identifier) @def.name
    (arguments) @def.params))
  (#any-of? @_kw "def" "defp" "defmacro" "defmacrop")) @def.function

; Zero-arg defs without parens: `def helper do ... end`.
(call
  target: (identifier) @_kw
  (arguments (identifier) @def.name)
  (#any-of? @_kw "def" "defp")) @def.function
