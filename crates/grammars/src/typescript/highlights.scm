; Variables
(identifier) @variable

(call_expression
  function: (member_expression
    object: (identifier) @type
    (#any-of? @type
      "Promise" "Array" "Object" "Map" "Set" "WeakMap" "WeakSet" "Date" "Error" "TypeError"
      "RangeError" "SyntaxError" "ReferenceError" "EvalError" "URIError" "RegExp" "Function"
      "Number" "String" "Boolean" "Symbol" "BigInt" "Proxy" "ArrayBuffer" "DataView")))

; Special identifiers
(type_annotation) @type

(type_identifier) @type

(predefined_type) @type.builtin

(type_alias_declaration
  (type_identifier) @type)

(type_alias_declaration
  value: (_
    (type_identifier) @type))

(interface_declaration
  (type_identifier) @type)

(class_declaration
  (type_identifier) @type.class)

(extends_clause
  value: (identifier) @type.class)

(extends_type_clause
  type: (type_identifier) @type)

(implements_clause
  (type_identifier) @type)

; Enables ts-pretty-errors
; The Lsp returns "snippets" of typescript, which are not valid typescript in totality,
; but should still be highlighted
; Highlights object literals by hijacking the statement_block pattern, but only if
; the statement block follows an object literal pattern
(statement_block
  (labeled_statement
    ; highlight the label like a property name
    label: (statement_identifier) @property.name
    body: [
      ; match a terminating expression statement
      (expression_statement
        ; single identifier - treat as a type name
        [
          (identifier) @type.name
          ; object - treat as a property - type pair
          (object
            (pair
              key: (_) @property.name
              value: (_) @type.name))
          ; subscript_expression - treat as an array declaration
          (subscript_expression
            object: (_) @type.name
            index: (_))
          ; templated string - treat each identifier contained as a type name
          (template_string
            (template_substitution
              (identifier) @type.name))
        ])
      ; match a nested statement block
      (statement_block) @nested
    ]))

; Inline type imports: import { type Foo } or import { type Foo as Bar }
(import_specifier
  "type"
  name: (identifier) @type)

(import_specifier
  "type"
  alias: (identifier) @type)

; Full type imports: import type { Foo } or import type { Foo as Bar }
(import_statement
  "type"
  (import_clause
    (named_imports
      (import_specifier
        name: (identifier) @type))))

(import_statement
  "type"
  (import_clause
    (named_imports
      (import_specifier
        alias: (identifier) @type))))

([
  (identifier)
  (shorthand_property_identifier)
  (shorthand_property_identifier_pattern)
] @constant
  (#match? @constant "^_*[A-Z_][A-Z\\d_]*$"))

; Properties
(property_identifier) @property

(shorthand_property_identifier) @property

(shorthand_property_identifier_pattern) @property

(private_property_identifier) @property

; Alternating chain colors for member access chains (e.g. a.b.c.d)
; The innermost property in a chain gets chain_1
(member_expression
  object: (member_expression
    property: (property_identifier) @property.chain_1))

(member_expression
  object: (member_expression
    property: (private_property_identifier) @property.chain_1))

; Properties whose object is a member_expression get chain_2
(member_expression
  object: (member_expression)
  property: (property_identifier) @property.chain_2)

(member_expression
  object: (member_expression)
  property: (private_property_identifier) @property.chain_2)

; Depth 2: override back to chain_1
(member_expression
  object: (member_expression
    object: (member_expression))
  property: (property_identifier) @property.chain_1)

(member_expression
  object: (member_expression
    object: (member_expression))
  property: (private_property_identifier) @property.chain_1)

; Depth 3: override back to chain_2
(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression)))
  property: (property_identifier) @property.chain_2)

(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression)))
  property: (private_property_identifier) @property.chain_2)

; Depth 4: override back to chain_1
(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression))))
  property: (property_identifier) @property.chain_1)

(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression))))
  property: (private_property_identifier) @property.chain_1)

; Depth 5: override back to chain_2
(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression)))))
  property: (property_identifier) @property.chain_2)

(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression)))))
  property: (private_property_identifier) @property.chain_2)

; Depth 6: override back to chain_1
(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression
            object: (member_expression))))))
  property: (property_identifier) @property.chain_1)

(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression
            object: (member_expression))))))
  property: (private_property_identifier) @property.chain_1)

; Depth 7: override back to chain_2
(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression
            object: (member_expression
              object: (member_expression)))))))
  property: (property_identifier) @property.chain_2)

(member_expression
  object: (member_expression
    object: (member_expression
      object: (member_expression
        object: (member_expression
          object: (member_expression
            object: (member_expression
              object: (member_expression)))))))
  property: (private_property_identifier) @property.chain_2)

; Function and method calls
(call_expression
  function: (identifier) @function)

(call_expression
  function: (member_expression
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method))

; Chained method calls - inner method of a chain gets chain_1
(call_expression
  function: (member_expression
    object: (call_expression
      function: (member_expression
        property: [
          (property_identifier)
          (private_property_identifier)
        ] @function.method.chain_1))))

; Method whose object is a chained call → chain_2
(call_expression
  function: (member_expression
    object: (call_expression
      function: (member_expression))
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method.chain_2))

; Depth 2: override back to chain_1
(call_expression
  function: (member_expression
    object: (call_expression
      function: (member_expression
        object: (call_expression
          function: (member_expression))))
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method.chain_1))

; Depth 3: override back to chain_2
(call_expression
  function: (member_expression
    object: (call_expression
      function: (member_expression
        object: (call_expression
          function: (member_expression
            object: (call_expression
              function: (member_expression))))))
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method.chain_2))

; Depth 4: override back to chain_1
(call_expression
  function: (member_expression
    object: (call_expression
      function: (member_expression
        object: (call_expression
          function: (member_expression
            object: (call_expression
              function: (member_expression
                object: (call_expression
                  function: (member_expression))))))))
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method.chain_1))

(new_expression
  constructor: (identifier) @type.class)

(nested_type_identifier
  module: (identifier) @type)

; Function and method definitions
(function_expression
  name: (identifier) @function)

(function_declaration
  name: (identifier) @function)

(method_definition
  name: [
    (property_identifier)
    (private_property_identifier)
  ] @function.method)

(method_definition
  name: (property_identifier) @constructor
  (#eq? @constructor "constructor"))

(pair
  key: [
    (property_identifier)
    (private_property_identifier)
  ] @function.method
  value: [
    (function_expression)
    (arrow_function)
  ])

(assignment_expression
  left: (member_expression
    property: [
      (property_identifier)
      (private_property_identifier)
    ] @function.method)
  right: [
    (function_expression)
    (arrow_function)
  ])

(variable_declarator
  name: (identifier) @function
  value: [
    (function_expression)
    (arrow_function)
  ])

(assignment_expression
  left: (identifier) @function
  right: [
    (function_expression)
    (arrow_function)
  ])

(arrow_function) @function

; Parameters
(required_parameter
  (identifier) @variable.parameter)

(required_parameter
  (_
    ([
      (identifier)
      (shorthand_property_identifier_pattern)
    ]) @variable.parameter))

(required_parameter
  (_
    (object_assignment_pattern
      left: (shorthand_property_identifier_pattern) @variable.parameter)))

(required_parameter
  (_
    (assignment_pattern
      left: (identifier) @variable.parameter)))

(optional_parameter
  (_
    (object_assignment_pattern
      left: (shorthand_property_identifier_pattern) @variable.parameter)))

(optional_parameter
  (_
    (assignment_pattern
      left: (identifier) @variable.parameter)))

(optional_parameter
  (identifier) @variable.parameter)

(optional_parameter
  (_
    ([
      (identifier)
      (shorthand_property_identifier_pattern)
    ]) @variable.parameter))

(catch_clause
  parameter: (identifier) @variable.parameter)

(index_signature
  name: (identifier) @variable.parameter)

(arrow_function
  parameter: (identifier) @variable.parameter)

(type_predicate
  name: (identifier) @variable.parameter)

; Rainbow parameter position colors (8-color cycle for declarations)
(required_parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-mod? @variable.parameter.chain_1 "8" "0"))

(required_parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-mod? @variable.parameter.chain_2 "8" "1"))

(required_parameter
  (identifier) @variable.parameter.chain_3
  (#sibling-index-mod? @variable.parameter.chain_3 "8" "2"))

(required_parameter
  (identifier) @variable.parameter.chain_4
  (#sibling-index-mod? @variable.parameter.chain_4 "8" "3"))

(required_parameter
  (identifier) @variable.parameter.chain_5
  (#sibling-index-mod? @variable.parameter.chain_5 "8" "4"))

(required_parameter
  (identifier) @variable.parameter.chain_6
  (#sibling-index-mod? @variable.parameter.chain_6 "8" "5"))

(required_parameter
  (identifier) @variable.parameter.chain_7
  (#sibling-index-mod? @variable.parameter.chain_7 "8" "6"))

(required_parameter
  (identifier) @variable.parameter.chain_8
  (#sibling-index-mod? @variable.parameter.chain_8 "8" "7"))

(optional_parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-mod? @variable.parameter.chain_1 "8" "0"))

(optional_parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-mod? @variable.parameter.chain_2 "8" "1"))

(optional_parameter
  (identifier) @variable.parameter.chain_3
  (#sibling-index-mod? @variable.parameter.chain_3 "8" "2"))

(optional_parameter
  (identifier) @variable.parameter.chain_4
  (#sibling-index-mod? @variable.parameter.chain_4 "8" "3"))

(optional_parameter
  (identifier) @variable.parameter.chain_5
  (#sibling-index-mod? @variable.parameter.chain_5 "8" "4"))

(optional_parameter
  (identifier) @variable.parameter.chain_6
  (#sibling-index-mod? @variable.parameter.chain_6 "8" "5"))

(optional_parameter
  (identifier) @variable.parameter.chain_7
  (#sibling-index-mod? @variable.parameter.chain_7 "8" "6"))

(optional_parameter
  (identifier) @variable.parameter.chain_8
  (#sibling-index-mod? @variable.parameter.chain_8 "8" "7"))

; Rainbow parameter reference colors (propagates declaration chain color to references)
((identifier) @variable.parameter.chain_1
  (#is-parameter-reference-mod? @variable.parameter.chain_1 "8" "0"))

((identifier) @variable.parameter.chain_2
  (#is-parameter-reference-mod? @variable.parameter.chain_2 "8" "1"))

((identifier) @variable.parameter.chain_3
  (#is-parameter-reference-mod? @variable.parameter.chain_3 "8" "2"))

((identifier) @variable.parameter.chain_4
  (#is-parameter-reference-mod? @variable.parameter.chain_4 "8" "3"))

((identifier) @variable.parameter.chain_5
  (#is-parameter-reference-mod? @variable.parameter.chain_5 "8" "4"))

((identifier) @variable.parameter.chain_6
  (#is-parameter-reference-mod? @variable.parameter.chain_6 "8" "5"))

((identifier) @variable.parameter.chain_7
  (#is-parameter-reference-mod? @variable.parameter.chain_7 "8" "6"))

((identifier) @variable.parameter.chain_8
  (#is-parameter-reference-mod? @variable.parameter.chain_8 "8" "7"))

; Nested closure depth coloring
(arrow_function
  "=>" @function.closure.depth_1
  (#ancestor-count-is-odd? @function.closure.depth_1))

(arrow_function
  "=>" @function.closure.depth_2
  (#ancestor-count-is-even? @function.closure.depth_2))

; Literals
(this) @variable.special

(super) @variable.special

[
  (null)
  (undefined)
] @constant.builtin

[
  (true)
  (false)
] @boolean

(literal_type
  [
    (null)
    (undefined)
    (true)
    (false)
  ] @type.builtin)

(comment) @comment

(hash_bang_line) @comment

[
  (string)
  (template_string)
  (template_literal_type)
] @string

(escape_sequence) @string.escape

(regex) @string.regex

(regex_flags) @keyword.operator.regex

(number) @number

; Tokens
[
  ";"
  "?."
  "."
  ","
  ":"
  "?"
] @punctuation.delimiter

[
  "..."
  "-"
  "--"
  "-="
  "+"
  "++"
  "+="
  "*"
  "*="
  "**"
  "**="
  "/"
  "/="
  "%"
  "%="
  "<"
  "<="
  "<<"
  "<<="
  "="
  "=="
  "==="
  "!"
  "!="
  "!=="
  "=>"
  ">"
  ">="
  ">>"
  ">>="
  ">>>"
  ">>>="
  "~"
  "^"
  "&"
  "|"
  "^="
  "&="
  "|="
  "&&"
  "||"
  "??"
  "&&="
  "||="
  "??="
  "..."
] @operator

(regex
  "/" @string.regex)

(ternary_expression
  [
    "?"
    ":"
  ] @operator)

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

(template_substitution
  "${" @punctuation.special
  "}" @punctuation.special) @embedded

(template_type
  "${" @punctuation.special
  "}" @punctuation.special) @embedded

(type_arguments
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

(type_parameters
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

(decorator
  "@" @punctuation.special)

(union_type
  "|" @punctuation.special)

(intersection_type
  "&" @punctuation.special)

(type_annotation
  ":" @punctuation.special)

(index_signature
  ":" @punctuation.special)

(type_predicate_annotation
  ":" @punctuation.special)

(public_field_definition
  "?" @punctuation.special)

(property_signature
  "?" @punctuation.special)

(method_signature
  "?" @punctuation.special)

(optional_parameter
  ([
    "?"
    ":"
  ]) @punctuation.special)

; Keywords
[
  "abstract"
  "as"
  "async"
  "debugger"
  "declare"
  "default"
  "delete"
  "extends"
  "get"
  "implements"
  "in"
  "infer"
  "instanceof"
  "is"
  "keyof"
  "module"
  "namespace"
  "new"
  "of"
  "override"
  "private"
  "protected"
  "public"
  "readonly"
  "satisfies"
  "set"
  "static"
  "target"
  "typeof"
  "using"
  "void"
  "with"
] @keyword

[
  "const"
  "let"
  "var"
  "function"
  "class"
  "enum"
  "interface"
  "type"
] @keyword.declaration

[
  "export"
  "from"
  "import"
] @keyword.import

[
  "await"
  "break"
  "case"
  "catch"
  "continue"
  "do"
  "else"
  "finally"
  "for"
  "if"
  "return"
  "switch"
  "throw"
  "try"
  "while"
  "yield"
] @keyword.control

(switch_default
  "default" @keyword.control)
