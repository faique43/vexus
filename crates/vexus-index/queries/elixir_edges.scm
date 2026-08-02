; Plain calls — minus the definition/control keywords, which are also
; (call) nodes in this grammar and would otherwise flood the edge list.
(call
  target: (identifier) @call.name
  (arguments) @call.args
  (#not-any-of? @call.name
    "def" "defp" "defmodule" "defmacro" "defmacrop" "defstruct"
    "defprotocol" "defimpl" "defdelegate" "defguard" "defguardp"
    "defexception" "import" "alias" "require" "use" "if" "unless"
    "case" "cond" "for" "with" "receive" "try" "raise" "quote")) @call

; `Enum.sum(...)` — the callee is the dot's right side.
(call
  target: (dot right: (identifier) @call.name)
  (arguments) @call.args) @call

(call
  target: (identifier) @_kw
  (arguments (alias) @import.module)
  (#any-of? @_kw "import" "alias" "require" "use"))
