# Rainbow Parameters: Technical Specification

## Overview

Rainbow Parameters is a syntax highlighting feature that assigns each function parameter a unique color based on its ordinal position in the parameter list, and then propagates that same color to every reference of that parameter within the function body. This makes it trivially easy to trace a parameter from its declaration through its uses, especially in functions with many parameters.

The feature extends Zed's existing chain-coloring system (used for property access chains and method call chains) to function parameters. It uses two alternating token scopes:

- `variable.parameter.chain_1` — even-indexed parameters (0th, 2nd, 4th, ...)
- `variable.parameter.chain_2` — odd-indexed parameters (1st, 3rd, 5th, ...)

The fallback `variable.parameter` scope remains in place and applies wherever the chain predicates do not match.

---

## Status in This Fork

**This feature is fully implemented.** The following components are all in place:

- Custom tree-sitter predicates (`#sibling-index-is-even?`, `#sibling-index-is-odd?`, `#is-parameter-reference-even?`, `#is-parameter-reference-odd?`) registered in `crates/language/src/syntax_map.rs`
- Highlight query patterns in `crates/languages/src/typescript/highlights.scm`, `javascript/highlights.scm`, `tsx/highlights.scm`, `rust/highlights.scm`, and `python/highlights.scm`
- Theme resolution via the existing `HighlightMap` longest-prefix matching in `crates/language/src/highlight_map.rs`

This document serves as a reference for understanding how the system works, how to extend it to additional languages, and what the known edge cases and limitations are.

---

## How Zed's Existing Chain System Works

### Property and Method Chain Coloring

Zed's property chain coloring (`property.chain_1`, `property.chain_2`) works by writing multiple tree-sitter query patterns of increasing depth, where each pattern overrides a shallower one. Since tree-sitter applies captures in order of specificity (more specific patterns win over less specific ones), a cascade of depth-matched patterns produces alternating colors:

```scheme
; Base: any property gets @property
(property_identifier) @property

; Depth 1: a property inside a member_expression chain gets chain_1
(member_expression
  object: (member_expression
    property: (property_identifier) @property.chain_1))

; Depth 1 outer: the outermost link of a 2-node chain gets chain_2
(member_expression
  object: (member_expression)
  property: (property_identifier) @property.chain_2)

; Depth 2: override back to chain_1
(member_expression
  object: (member_expression
    object: (member_expression))
  property: (property_identifier) @property.chain_1)

; ... and so on for 7 more depths
```

This technique is purely structural: it relies on the nesting shape of the AST to determine position. It works well for chains because the nesting depth directly encodes position.

### Why This Approach Cannot Work for Parameters

Function parameters are siblings, not nested nodes. The formal parameter list `(a, b, c)` produces a flat list of sibling nodes under `formal_parameters`, not a nested chain. There is no structural depth difference between the first and fourth parameter. Ordinal position must be computed by counting siblings — which requires a custom tree-sitter predicate, not just pattern nesting.

---

## Architecture: Custom Predicates

Standard tree-sitter predicates (like `#eq?`, `#match?`, `#any-of?`) are evaluated by the tree-sitter library itself. Zed also supports *general predicates* — unknown predicates that tree-sitter passes through to the host application for evaluation.

The entry point is `satisfies_custom_predicates` in `crates/language/src/syntax_map.rs`, called during highlight iteration. It receives the query, the current match, and the rope text, and dispatches on the predicate operator string.

### Predicate: `#sibling-index-is-even?` / `#sibling-index-is-odd?`

**Purpose:** Determines whether the captured node's *parent* is at an even or odd position among its siblings under the grandparent.

**Why parent, not node itself?** For a pattern like `(required_parameter (identifier) @capture)`, the captured node is the `identifier` — a child of the `required_parameter`. The positional index we care about is the `required_parameter`'s index among `formal_parameters`, not the `identifier`'s index inside the `required_parameter`. The predicate therefore walks up two levels: node → parent → grandparent, then counts siblings of the parent.

**Implementation (`sibling_index_parity`):**

```rust
fn sibling_index_parity(args: &[QueryPredicateArg], mat: &QueryMatch, want_odd: bool) -> bool {
    let node = resolve_capture(args, mat)?.node;
    let parent = node.parent()?;
    let grandparent = parent.parent()?;
    let mut index = 0;
    let mut cursor = grandparent.walk();
    for child in grandparent.named_children(&mut cursor) {
        if child.id() == parent.id() {
            return (index % 2 != 0) == want_odd;
        }
        index += 1;
    }
    false
}
```

Note that `named_children` is used — unnamed nodes (punctuation like commas and parentheses) are excluded from the count, so parameter indices are contiguous integers with no gaps.

### Predicate: `#is-parameter-reference-even?` / `#is-parameter-reference-odd?`

**Purpose:** Determines whether a given `identifier` node in the function body is a reference to a parameter declared at an even or odd position.

**Implementation (`is_parameter_reference`):**

1. Confirm the captured node is an `identifier`.
2. Extract its text from the rope.
3. Walk up the AST until a function boundary node is found (`arrow_function`, `function_declaration`, `function_expression`, `method_definition`, `function`, `generator_function`, `generator_function_declaration`).
4. Within that function node, find its `formal_parameters` child.
5. Iterate the named children of `formal_parameters`, counting `required_parameter` and `optional_parameter` nodes. For each, extract the parameter name via `find_parameter_identifier`.
6. If the extracted name matches the reference text, return whether that parameter's index has the desired parity.

**`find_parameter_identifier`** extracts the name node from a parameter node by trying, in order:
1. The `pattern` field (TypeScript `required_parameter` uses this for the binding identifier)
2. The `name` field
3. The first `identifier` named child

---

## Highlight Query Patterns

### TypeScript / JavaScript (`typescript/highlights.scm`, `javascript/highlights.scm`)

```scheme
; Parameters
(required_parameter
  (identifier) @variable.parameter)

(optional_parameter
  (identifier) @variable.parameter)

(arrow_function
  parameter: (identifier) @variable.parameter)

; Alternating parameter position colors
(required_parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-is-even? @variable.parameter.chain_1))

(required_parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-is-odd? @variable.parameter.chain_2))

(optional_parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-is-even? @variable.parameter.chain_1))

(optional_parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-is-odd? @variable.parameter.chain_2))

; Parameter reference chain coloring (propagates declaration chain color to references)
(identifier) @variable.parameter.chain_1
  (#is-parameter-reference-even? @variable.parameter.chain_1)

(identifier) @variable.parameter.chain_2
  (#is-parameter-reference-odd? @variable.parameter.chain_2)
```

The `@variable.parameter` base patterns fire first and establish the fallback. The `chain_1`/`chain_2` patterns are more specific captures of the same nodes, so Zed's highlight system (which takes the longest matching theme key) will prefer them when the predicate passes.

### Rust (`rust/highlights.scm`)

```scheme
(parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-is-even? @variable.parameter.chain_1))

(parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-is-odd? @variable.parameter.chain_2))
```

Rust uses `parameter` (not `required_parameter`) as the node kind.

### Python (`python/highlights.scm`)

```scheme
(function_definition
  parameters: (parameters
    (identifier) @variable.parameter.chain_1
    (#sibling-index-is-even? @variable.parameter.chain_1)))

(function_definition
  parameters: (parameters
    (identifier) @variable.parameter.chain_2
    (#sibling-index-is-odd? @variable.parameter.chain_2)))

(function_definition
  parameters: (parameters
    (typed_parameter
      (identifier) @variable.parameter.chain_1
      (#sibling-index-is-even? @variable.parameter.chain_1))))

(function_definition
  parameters: (parameters
    (typed_parameter
      (identifier) @variable.parameter.chain_2
      (#sibling-index-is-odd? @variable.parameter.chain_2))))
```

Python requires separate patterns for plain identifiers and `typed_parameter` nodes.

---

## Theme Resolution

Theme key matching in `HighlightMap::new` uses a longest-prefix algorithm: for a capture name like `variable.parameter.chain_1`, the theme key that shares the most dot-separated components wins. The matching is order-independent across components (any component of the key must appear somewhere in the capture name), but the winning key is the one with the most matching components.

This means:

| Capture | Theme keys present | Resolved to |
|---|---|---|
| `variable.parameter.chain_1` | `variable`, `variable.parameter`, `variable.parameter.chain_1` | `variable.parameter.chain_1` |
| `variable.parameter.chain_1` | `variable`, `variable.parameter` | `variable.parameter` |
| `variable.parameter` | `variable`, `variable.parameter` | `variable.parameter` |

The theme does not need to define `chain_1`/`chain_2` keys for the feature to work; if they are absent, captures fall back gracefully to `variable.parameter`.

---

## Edge Cases and Limitations

### Destructured Parameters

Parameters using object or array destructuring (e.g., `{ a, b }: Config`) are not colored per-parameter by the current implementation. The `find_parameter_identifier` function only extracts a single name node from each parameter node. For a destructured parameter, the binding contains multiple identifiers.

Behavior: the parameter slot still occupies its ordinal position and its slot color is assigned, but only the top-level binding pattern (if it is a plain identifier) is matched. Destructured sub-bindings (`a`, `b` inside `{ a, b }`) will not be colored with the parameter's chain color — they will fall through to `@variable.parameter` or `@variable`.

**To fix:** Extend `find_parameter_identifier` to return all leaf identifiers within a destructuring pattern, and update `function_parameter_index` to map each leaf identifier name to the parent parameter's index.

### Rest Parameters

Rest parameters (`...args`) parse as `rest_pattern` containing an `identifier`. The current `find_parameter_identifier` fallback (first `identifier` named child) will find this identifier, so `args` will be assigned a chain color based on its ordinal slot. References to `args` in the body will also be colored correctly via `is_parameter_reference`.

### Default Value Parameters

Parameters with defaults (`x = 0`) parse as `assignment_pattern` inside `required_parameter`. The `find_parameter_identifier` tries the `pattern` field first, which should resolve to the identifier before the `=`. This should work correctly in most cases, but verify with the actual tree-sitter grammar for your target language version.

### Single-Parameter Arrow Functions

Arrow functions with a single parameter and no parentheses (`x => x + 1`) use a different AST node: the `arrow_function` has a `parameter` field (a bare `identifier`), not a `formal_parameters` node. The current `function_parameter_index` implementation searches for `formal_parameters` and will return `None` for this form.

Behavior: the parameter `x` declared this way does not receive a chain color at the declaration site (it only receives `@variable.parameter` from the `(arrow_function parameter: (identifier) @variable.parameter)` pattern). References to `x` inside the body also will not receive chain coloring.

**To fix:** Add a branch in `function_parameter_index` that handles the case where the function node has a `parameter` field instead of `formal_parameters`.

### Nested Functions and Closures

The `is_parameter_reference` predicate walks up the AST and stops at the *nearest* enclosing function boundary. This is correct behavior: a reference inside a nested function or closure is scoped to that nested function, not the outer one. If an inner function declares its own parameter with the same name as an outer parameter, the inner declaration shadows the outer, and references inside the inner function are attributed to the inner parameter's index — which is correct.

If an inner function captures an outer parameter by reference (a closure capture, not a re-declaration), the walk will stop at the inner function boundary, find no matching parameter in its list, and return `false`. The reference will then fall through to `@variable.parameter` or `@variable`. This is a known limitation: closure captures of outer parameters are not colored with the outer parameter's chain color.

**To fix:** When `function_parameter_index` returns `None` for the innermost function, continue walking outward to look for the name in enclosing function parameter lists. This requires understanding JavaScript/TypeScript scoping rules, which is non-trivial to do accurately in a tree-sitter predicate without full semantic analysis.

### TypeScript-Only: `this` Parameter

TypeScript allows a fake `this` parameter as the first formal parameter to annotate the type of `this`. This parameter occupies index 0 and would receive `chain_1` coloring. This is generally acceptable but could be surprising. If desired, filter it out in `function_parameter_index` by name.

### Methods with Implicit `self` (Rust, Python)

In Rust, `self` and `&self` are `self_parameter` nodes, not `parameter` nodes, so they are excluded from the sibling count automatically. In Python, `self` and `cls` are plain `identifier` nodes at index 0, so they will receive `chain_1` coloring. Whether this is desired is a matter of preference.

---

## How to Extend to a New Language

1. **Determine the grammar node kinds** for the target language's function declarations and parameter lists. Use `tree-sitter parse` or the playground at `tree-sitter.github.io/tree-sitter/playground` to inspect the AST for representative code.

2. **Identify the parameter node kind** — the node that wraps each individual parameter within the list (e.g., `required_parameter` in TS, `parameter` in Rust, bare `identifier` or `typed_parameter` in Python).

3. **Write the declaration-site query patterns** for both `chain_1` (even) and `chain_2` (odd) using `#sibling-index-is-even?` and `#sibling-index-is-odd?`. Add these to the language's `highlights.scm`.

4. **Write the reference-site query patterns** using `#is-parameter-reference-even?` and `#is-parameter-reference-odd?`. Add an `(identifier)` capture with each predicate.

5. **Update `is_parameter_reference` in `syntax_map.rs`** to add the new language's function node kinds to the `match` arm (if they are not already covered). The existing set covers JavaScript, TypeScript, and Rust. Python, Go, and others would need their own node kinds added.

6. **Update `function_parameter_index` in `syntax_map.rs`** to handle the new language's parameter node kind names in the `match child.kind()` block.

---

## Key Files

| File | Role |
|---|---|
| `crates/language/src/syntax_map.rs` | Custom predicate dispatch and implementation (`satisfies_custom_predicates`, `sibling_index_parity`, `is_parameter_reference`, `function_parameter_index`, `find_parameter_identifier`) |
| `crates/language/src/highlight_map.rs` | Theme key resolution via longest-prefix matching |
| `crates/languages/src/typescript/highlights.scm` | TS parameter chain query patterns |
| `crates/languages/src/javascript/highlights.scm` | JS parameter chain query patterns |
| `crates/languages/src/tsx/highlights.scm` | TSX parameter chain query patterns |
| `crates/languages/src/rust/highlights.scm` | Rust parameter chain query patterns |
| `crates/languages/src/python/highlights.scm` | Python parameter chain query patterns |

---

## Implementation Step-by-Step (Original Design)

This section documents the sequence in which this feature was designed to be built, for reference when implementing the same feature in other editors or from scratch.

### Step 1: Add Custom Predicate Infrastructure

In the syntax highlight loop, intercept `general_predicates` on each query match. Implement a dispatch function that maps predicate operator strings to Rust functions. This is the extension point that all subsequent steps depend on.

### Step 2: Implement `sibling_index_parity`

Implement the predicate function that, given a captured node, walks to its parent and grandparent and counts the parent's index among named siblings. This is sufficient to color parameters at their declaration site.

### Step 3: Add Declaration-Site Query Patterns

For each supported language, add `chain_1` and `chain_2` capture patterns to `highlights.scm` with the `sibling-index-is-even?` / `sibling-index-is-odd?` predicates. Verify in the editor that parameter declarations alternate colors.

### Step 4: Implement `is_parameter_reference`

Implement the predicate that resolves an identifier to a parameter declaration. This requires: walking up the AST to the enclosing function, locating the parameter list, and matching the identifier text against parameter names. This is the highest-complexity step.

### Step 5: Add Reference-Site Query Patterns

Add `(identifier) @variable.parameter.chain_1 (#is-parameter-reference-even? ...)` and the odd variant to `highlights.scm` for each language. Verify that body references match their declaration colors.

### Step 6: Define Theme Keys

Add `variable.parameter.chain_1` and `variable.parameter.chain_2` to the theme's syntax highlight list with distinct colors. Because `HighlightMap` uses longest-prefix matching, these keys automatically take priority over the base `variable.parameter` key when chain predicates match.

### Step 7: Validate Edge Cases

Test destructuring, rest parameters, default values, single-param arrow functions, nested closures, and shadowing. Document any unhandled cases.
