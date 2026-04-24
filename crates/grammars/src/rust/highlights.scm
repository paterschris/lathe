(identifier) @variable

(metavariable) @variable

(type_identifier) @type

(fragment_specifier) @type

(primitive_type) @type.builtin

(self) @variable.special

(field_identifier) @property

(shorthand_field_identifier) @property

; Alternating chain colors for field access chains (e.g. a.b.c.d)
; The innermost field in a chain gets chain_1
(field_expression
  value: (field_expression
    field: (field_identifier) @property.chain_1))

; Fields whose value is a field_expression get chain_2
(field_expression
  value: (field_expression)
  field: (field_identifier) @property.chain_2)

; Depth 2: override back to chain_1
(field_expression
  value: (field_expression
    value: (field_expression))
  field: (field_identifier) @property.chain_1)

; Depth 3: override back to chain_2
(field_expression
  value: (field_expression
    value: (field_expression
      value: (field_expression)))
  field: (field_identifier) @property.chain_2)

; Depth 4: override back to chain_1
(field_expression
  value: (field_expression
    value: (field_expression
      value: (field_expression
        value: (field_expression))))
  field: (field_identifier) @property.chain_1)

; Depth 5: override back to chain_2
(field_expression
  value: (field_expression
    value: (field_expression
      value: (field_expression
        value: (field_expression
          value: (field_expression)))))
  field: (field_identifier) @property.chain_2)

; Depth 6: override back to chain_1
(field_expression
  value: (field_expression
    value: (field_expression
      value: (field_expression
        value: (field_expression
          value: (field_expression
            value: (field_expression))))))
  field: (field_identifier) @property.chain_1)

; Depth 7: override back to chain_2
(field_expression
  value: (field_expression
    value: (field_expression
      value: (field_expression
        value: (field_expression
          value: (field_expression
            value: (field_expression
              value: (field_expression)))))))
  field: (field_identifier) @property.chain_2)

(trait_item
  name: (type_identifier) @type.interface)

(impl_item
  trait: (type_identifier) @type.interface)

(abstract_type
  trait: (type_identifier) @type.interface)

(dynamic_type
  trait: (type_identifier) @type.interface)

(trait_bounds
  (type_identifier) @type.interface)

(call_expression
  function: [
    (identifier) @function
    (scoped_identifier
      name: (identifier) @function)
    (field_expression
      field: (field_identifier) @function.method)
  ])

(generic_function
  function: [
    (identifier) @function
    (scoped_identifier
      name: (identifier) @function)
    (field_expression
      field: (field_identifier) @function.method)
  ])

; Chained method calls - inner method of a chain gets chain_1
(call_expression
  function: (field_expression
    value: (call_expression
      function: (field_expression
        field: (field_identifier) @function.method.chain_1))))

; Method whose value is a chained call → chain_2
(call_expression
  function: (field_expression
    value: (call_expression
      function: (field_expression))
    field: (field_identifier) @function.method.chain_2))

; Depth 2: override back to chain_1
(call_expression
  function: (field_expression
    value: (call_expression
      function: (field_expression
        value: (call_expression
          function: (field_expression))))
    field: (field_identifier) @function.method.chain_1))

; Depth 3: override back to chain_2
(call_expression
  function: (field_expression
    value: (call_expression
      function: (field_expression
        value: (call_expression
          function: (field_expression
            value: (call_expression
              function: (field_expression))))))
    field: (field_identifier) @function.method.chain_2))

; Depth 4: override back to chain_1
(call_expression
  function: (field_expression
    value: (call_expression
      function: (field_expression
        value: (call_expression
          function: (field_expression
            value: (call_expression
              function: (field_expression
                value: (call_expression
                  function: (field_expression))))))))
    field: (field_identifier) @function.method.chain_1))

(function_item
  name: (identifier) @function.definition)

(function_signature_item
  name: (identifier) @function.definition)

(macro_invocation
  macro: [
    (identifier) @function.special
    (scoped_identifier
      name: (identifier) @function.special)
  ])

(macro_invocation
  "!" @function.special)

(macro_definition
  name: (identifier) @function.special.definition)

; Identifier conventions
; Assume uppercase names are types/enum-constructors
((identifier) @type
  (#match? @type "^[A-Z]"))

; Assume all-caps names are constants
((identifier) @constant
  (#match? @constant "^_*[A-Z][A-Z\\d_]*$"))

; Ensure enum variants are highlighted correctly regardless of naming convention
(enum_variant
  name: (identifier) @type)

[
  "("
  ")"
  "{"
  "}"
  "["
  "]"
] @punctuation.bracket

(_
  .
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

[
  "."
  ";"
  ","
  "::"
] @punctuation.delimiter

"#" @punctuation.special

[
  "as"
  "async"
  "const"
  "default"
  "dyn"
  "enum"
  "extern"
  "fn"
  "impl"
  "let"
  "macro_rules!"
  "mod"
  "move"
  "pub"
  "raw"
  "ref"
  "static"
  "struct"
  "for"
  "trait"
  "type"
  "union"
  "unsafe"
  "use"
  "where"
  (crate)
  (mutable_specifier)
  (super)
] @keyword

[
  "await"
  "break"
  "continue"
  "else"
  "if"
  "in"
  "loop"
  "match"
  "return"
  "while"
  "yield"
] @keyword.control

(for_expression
  "for" @keyword.control)

[
  (string_literal)
  (raw_string_literal)
  (char_literal)
] @string

(escape_sequence) @string.escape

[
  (integer_literal)
  (float_literal)
] @number

(boolean_literal) @boolean

[
  (line_comment)
  (block_comment)
] @comment

[
  (line_comment
    (doc_comment))
  (block_comment
    (doc_comment))
] @comment.doc

[
  "!="
  "%"
  "%="
  "&"
  "&="
  "&&"
  "*"
  "*="
  "+"
  "+="
  "-"
  "-="
  "->"
  ".."
  "..="
  "..."
  "/="
  ":"
  "<<"
  "<<="
  "<"
  "<="
  "="
  "=="
  "=>"
  ">"
  ">="
  ">>"
  ">>="
  "@"
  "^"
  "^="
  "|"
  "|="
  "||"
  "?"
] @operator

; Avoid highlighting these as operators when used in doc comments.
(unary_expression
  "!" @operator)

operator: "/" @operator

(lifetime
  "'" @lifetime
  (identifier) @lifetime)

(parameter
  (identifier) @variable.parameter)

; Alternating parameter position colors
(parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-is-even? @variable.parameter.chain_1))

(parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-is-odd? @variable.parameter.chain_2))

; Nested closure depth coloring
(closure_expression
  (closure_parameters
    "|" @function.closure.depth_1)
  (#ancestor-count-is-odd? @function.closure.depth_1))

(closure_expression
  (closure_parameters
    "|" @function.closure.depth_2)
  (#ancestor-count-is-even? @function.closure.depth_2))

(attribute_item
  (attribute
    [
      (identifier) @attribute
      (scoped_identifier
        name: (identifier) @attribute)
      (token_tree
        (identifier) @attribute
        (#match? @attribute "^[a-z\\d_]*$"))
      (token_tree
        (identifier) @none
        "::"
        (#match? @none "^[a-z\\d_]*$"))
    ]))

(inner_attribute_item
  (attribute
    [
      (identifier) @attribute
      (scoped_identifier
        name: (identifier) @attribute)
      (token_tree
        (identifier) @attribute
        (#match? @attribute "^[a-z\\d_]*$"))
      (token_tree
        (identifier) @none
        "::"
        (#match? @none "^[a-z\\d_]*$"))
    ]))
