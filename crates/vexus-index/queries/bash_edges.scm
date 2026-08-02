; Conservative: every command invocation names a potential callee; script-
; local function calls resolve, everything else stays unresolved (and the
; graph tools collapse those). Shell builtins and control noise excluded.
(command
  name: (command_name (word) @call.name)
  (#not-any-of? @call.name
    "set" "echo" "printf" "cd" "exit" "export" "local" "return" "shift"
    "source" "trap" "test" "[" "[[" "read" "unset" "eval" "exec" "wait"
    "true" "false" "break" "continue")) @call
