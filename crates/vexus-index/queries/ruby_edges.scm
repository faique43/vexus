; require/require_relative are captured as imports below, so they're
; excluded here rather than double-reported as calls.
(call
  method: (identifier) @call.name
  arguments: (argument_list) @call.args
  (#not-any-of? @call.name "require" "require_relative" "include" "extend"
    "attr_accessor" "attr_reader" "attr_writer")) @call

(call
  method: (identifier) @_kw
  arguments: (argument_list (string (string_content) @import.module))
  (#any-of? @_kw "require" "require_relative"))
