(identifier) @variable

(type_identifier) @type

(type_spec
  name: (type_identifier) @type.definition)

(field_identifier) @property

; Alternating chain colors for selector chains (e.g. a.b.c.d)
; The innermost field in a chain gets chain_1
(selector_expression
  operand: (selector_expression
    field: (field_identifier) @property.chain_1))

; Fields whose operand is a selector_expression get chain_2
(selector_expression
  operand: (selector_expression)
  field: (field_identifier) @property.chain_2)

; Depth 2: override back to chain_1
(selector_expression
  operand: (selector_expression
    operand: (selector_expression))
  field: (field_identifier) @property.chain_1)

; Depth 3: override back to chain_2
(selector_expression
  operand: (selector_expression
    operand: (selector_expression
      operand: (selector_expression)))
  field: (field_identifier) @property.chain_2)

; Depth 4: override back to chain_1
(selector_expression
  operand: (selector_expression
    operand: (selector_expression
      operand: (selector_expression
        operand: (selector_expression))))
  field: (field_identifier) @property.chain_1)

; Depth 5: override back to chain_2
(selector_expression
  operand: (selector_expression
    operand: (selector_expression
      operand: (selector_expression
        operand: (selector_expression
          operand: (selector_expression)))))
  field: (field_identifier) @property.chain_2)

; Depth 6: override back to chain_1
(selector_expression
  operand: (selector_expression
    operand: (selector_expression
      operand: (selector_expression
        operand: (selector_expression
          operand: (selector_expression
            operand: (selector_expression))))))
  field: (field_identifier) @property.chain_1)

; Depth 7: override back to chain_2
(selector_expression
  operand: (selector_expression
    operand: (selector_expression
      operand: (selector_expression
        operand: (selector_expression
          operand: (selector_expression
            operand: (selector_expression
              operand: (selector_expression)))))))
  field: (field_identifier) @property.chain_2)

(package_identifier) @namespace

(label_name) @label

(keyed_element
  .
  (literal_element
    (identifier) @property))

(call_expression
  function: (identifier) @function.call)

(call_expression
  function: (selector_expression
    field: (field_identifier) @function.method.call))

; Chained method calls - inner method of a chain gets chain_1
(call_expression
  function: (selector_expression
    operand: (call_expression
      function: (selector_expression
        field: (field_identifier) @function.method.call.chain_1))))

; Method whose operand is a chained call → chain_2
(call_expression
  function: (selector_expression
    operand: (call_expression
      function: (selector_expression))
    field: (field_identifier) @function.method.call.chain_2))

; Depth 2: override back to chain_1
(call_expression
  function: (selector_expression
    operand: (call_expression
      function: (selector_expression
        operand: (call_expression
          function: (selector_expression))))
    field: (field_identifier) @function.method.call.chain_1))

; Depth 3: override back to chain_2
(call_expression
  function: (selector_expression
    operand: (call_expression
      function: (selector_expression
        operand: (call_expression
          function: (selector_expression
            operand: (call_expression
              function: (selector_expression))))))
    field: (field_identifier) @function.method.call.chain_2))

; Depth 4: override back to chain_1
(call_expression
  function: (selector_expression
    operand: (call_expression
      function: (selector_expression
        operand: (call_expression
          function: (selector_expression
            operand: (call_expression
              function: (selector_expression
                operand: (call_expression
                  function: (selector_expression))))))))
    field: (field_identifier) @function.method.call.chain_1))

(function_declaration
  name: (identifier) @function)

(method_declaration
  name: (field_identifier) @function.method)

(method_elem
  name: (field_identifier) @function.method)

[
  ";"
  "."
  ","
  ":"
] @punctuation.delimiter

[
  "("
  ")"
  "{"
  "}"
  "["
  "]"
] @punctuation.bracket

[
  "--"
  "-"
  "-="
  ":="
  "!"
  "!="
  "..."
  "*"
  "*"
  "*="
  "/"
  "/="
  "&"
  "&&"
  "&="
  "%"
  "%="
  "^"
  "^="
  "+"
  "++"
  "+="
  "<-"
  "<"
  "<<"
  "<<="
  "<="
  "="
  "=="
  ">"
  ">="
  ">>"
  ">>="
  "|"
  "|="
  "||"
  "~"
] @operator

[
  "break"
  "case"
  "chan"
  "const"
  "continue"
  "default"
  "defer"
  "else"
  "fallthrough"
  "for"
  "func"
  "go"
  "goto"
  "if"
  "import"
  "interface"
  "map"
  "package"
  "range"
  "return"
  "select"
  "struct"
  "switch"
  "type"
  "var"
] @keyword

[
  (interpreted_string_literal)
  (raw_string_literal)
  (rune_literal)
] @string

(escape_sequence) @string.escape

[
  (int_literal)
  (float_literal)
  (imaginary_literal)
] @number

(const_spec
  name: (identifier) @constant)

[
  (true)
  (false)
] @boolean

[
  (nil)
  (iota)
] @constant.builtin

(comment) @comment

; Go directives
((comment) @preproc
  (#match? @preproc "^//go:"))

((comment) @preproc
  (#match? @preproc "^// \\+build"))
