# Rainbow Parameters: Technical Specification

## Overview

Rainbow Parameters is a syntax highlighting feature that assigns each function parameter a color based on its ordinal position in the parameter list, and then propagates that same color to every reference of that parameter within the function body. This makes it trivially easy to trace a parameter from its declaration through its uses, especially in functions with many parameters.

The feature extends Zed's existing chain-coloring system (used for property access chains and method call chains) to function parameters. Two coloring schemes are in the query files:

| Languages | Scheme | Scopes |
|---|---|---|
| TypeScript, TSX, JavaScript | 8-color cycle by `index % 8` | `variable.parameter.chain_1` through `chain_8` |
| Rust, Python | 2-color alternation by index parity | `variable.parameter.chain_1`, `variable.parameter.chain_2` |

The fallback `variable.parameter` scope remains in place and applies wherever the chain predicates do not match.

---

## Status in This Fork

**The feature is currently inert. The query patterns ship, but the Rust predicates that drive them do not exist.**

Three components are needed for rainbow parameters to render:

| Component | State |
|---|---|
| Highlight query patterns in `crates/grammars/src/*/highlights.scm` | ✅ Present |
| Custom tree-sitter predicates in `crates/language/src/syntax_map.rs` | ❌ Missing |
| `variable.parameter.chain_N` keys in a theme | ❌ Not defined by any bundled theme |

### What happened

Commit `f1b6ffa675` ("Release v0.236.14 stable", 2026-06-04) folded a 382-commit upstream merge into the fork. It took upstream's `crates/language/src/syntax_map.rs` wholesale, which dropped every custom predicate except `has-parent?` / `not-has-parent?`. The `.scm` query files kept their rainbow patterns, so the two halves are now out of sync.

The predicates removed by that commit:

- `#sibling-index-is-even?` / `#sibling-index-is-odd?`
- `#sibling-index-mod?`
- `#is-parameter-reference-even?` / `#is-parameter-reference-odd?`
- `#is-parameter-reference-mod?`
- `#ancestor-count-is-even?` / `#ancestor-count-is-odd?` (used for closure-pipe depth coloring, not parameters, but broken by the same removal)

### Why nothing visibly broke

`satisfies_custom_predicates` treats an unrecognized predicate as satisfied:

```rust
let satisfied = match predicate.operator.as_ref() {
    "has-parent?" => has_parent(&predicate.args, mat),
    "not-has-parent?" => !has_parent(&predicate.args, mat),
    _ => true,
};
```

So all eight `chain_N` patterns match every parameter unconditionally rather than one matching per position. That would normally produce a visible bug (every parameter taking the last-matching pattern's color), except that no bundled theme defines `variable.parameter.chain_N` keys. `HighlightMap`'s longest-prefix matching resolves every `chain_N` capture back to plain `variable.parameter`, so all parameters render in the same color and the breakage stays invisible.

### To restore

1. Reinstate the predicate functions in `crates/language/src/syntax_map.rs`. They can be recovered verbatim from `git show f1b6ffa675^:crates/language/src/syntax_map.rs` (lines 1435-1730 in that revision): `resolve_capture`, `sibling_index_parity`, `sibling_index_mod`, `ancestor_count_parity`, `is_parameter_reference`, `is_parameter_reference_mod`, `function_parameter_index`, `find_parameter_identifier`.
2. Restore the dispatch arms in `satisfies_custom_predicates`, and restore its `text: &Rope` parameter. The reference-site predicates need the rope to read identifier text; upstream's current signature omits it, so every call site needs the argument threaded back through.
3. Define `variable.parameter.chain_1` through `chain_8` in the theme. Until these keys exist, the feature is a no-op even with working predicates.

This document describes the design as it was built, so it doubles as the restoration reference.

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

Function parameters are siblings, not nested nodes. The formal parameter list `(a, b, c)` produces a flat list of sibling nodes under `formal_parameters`, not a nested chain. There is no structural depth difference between the first and fourth parameter. Ordinal position must be computed by counting siblings, which requires a custom tree-sitter predicate, not just pattern nesting.

---

## Architecture: Custom Predicates

Standard tree-sitter predicates (like `#eq?`, `#match?`, `#any-of?`) are evaluated by the tree-sitter library itself. Zed also supports *general predicates*: unknown predicates that tree-sitter passes through to the host application for evaluation.

The entry point is `satisfies_custom_predicates` in `crates/language/src/syntax_map.rs`, called during highlight iteration. It receives the query, the current match, and (in the fork's version) the rope text, and dispatches on the predicate operator string.

### Predicate: `#sibling-index-mod?`

**Signature:** `(#sibling-index-mod? @capture "modulus" "remainder")`

**Purpose:** True when the captured node's *parent* sits at an index among its named siblings such that `index % modulus == remainder`. This is the generalization used by the 8-color TS/TSX/JS patterns; `"8" "0"` is the first parameter, `"8" "1"` the second, and so on, wrapping at the ninth.

**Why parent, not node itself?** For a pattern like `(required_parameter (identifier) @capture)`, the captured node is the `identifier`, a child of the `required_parameter`. The positional index we care about is the `required_parameter`'s index among `formal_parameters`, not the `identifier`'s index inside the `required_parameter`. The predicate therefore walks up two levels: node → parent → grandparent, then counts siblings of the parent.

```rust
fn sibling_index_mod(args: &[QueryPredicateArg], mat: &QueryMatch) -> bool {
    let capture = resolve_capture(args, mat)?;
    let (modulus, remainder) = parse_string_args(args)?;   // args[1], args[2]
    if modulus == 0 {
        return false;
    }
    let parent = capture.node.parent()?;
    let grandparent = parent.parent()?;
    let mut cursor = grandparent.walk();
    for (index, child) in grandparent.named_children(&mut cursor).enumerate() {
        if child.id() == parent.id() {
            return index % modulus == remainder;
        }
    }
    false
}
```

Note that `named_children` is used: unnamed nodes (punctuation like commas and parentheses) are excluded from the count, so parameter indices are contiguous integers with no gaps.

### Predicate: `#sibling-index-is-even?` / `#sibling-index-is-odd?`

The parity special case of the above, equivalent to a modulus of 2. Rust and Python use these rather than the mod form, which is why those languages alternate between two colors instead of cycling through eight.

### Predicate: `#is-parameter-reference-mod?`

**Signature:** `(#is-parameter-reference-mod? @capture "modulus" "remainder")`

**Purpose:** Determines whether a given `identifier` node in the function body is a reference to a parameter whose declared position satisfies `index % modulus == remainder`. This is what propagates a declaration's color to its uses.

**Implementation:**

1. Confirm the captured node is an `identifier`.
2. Extract its text from the rope.
3. Walk up the AST until a function boundary node is found (`arrow_function`, `function_declaration`, `function_expression`, `method_definition`, `function`, `generator_function`, `generator_function_declaration`).
4. Within that function node, find its `formal_parameters` child.
5. Iterate the named children of `formal_parameters`, counting `required_parameter` and `optional_parameter` nodes. For each, extract the parameter name via `find_parameter_identifier`.
6. If the extracted name matches the reference text, return whether that parameter's index satisfies the modulus test.

**`find_parameter_identifier`** extracts the name node from a parameter node by trying, in order:
1. The `pattern` field (TypeScript `required_parameter` uses this for the binding identifier)
2. The `name` field
3. The first `identifier` named child

### Predicate: `#is-parameter-reference-even?` / `#is-parameter-reference-odd?`

The parity special case, paired with `#sibling-index-is-even?` / `#sibling-index-is-odd?`.

---

## Highlight Query Patterns

Queries live in `crates/grammars/src/<language>/highlights.scm`. They moved there from `crates/languages/src/` when upstream extracted the `language_core` and `grammars` crates (`3ce0cd11ec`).

### TypeScript / TSX / JavaScript

Sixteen declaration-site patterns (eight for `required_parameter`, eight for `optional_parameter`) plus eight reference-site patterns:

```scheme
; Parameters (base fallback)
(required_parameter
  (identifier) @variable.parameter)

(optional_parameter
  (identifier) @variable.parameter)

(arrow_function
  parameter: (identifier) @variable.parameter)

; Rainbow parameter position colors (8-color cycle for declarations)
(required_parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-mod? @variable.parameter.chain_1 "8" "0"))

(required_parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-mod? @variable.parameter.chain_2 "8" "1"))

; ... chain_3 through chain_8 with remainders "2" through "7",
; then the same eight again for (optional_parameter ...)

; Reference-site: propagate the declaration's color to every use
(identifier) @variable.parameter.chain_1
  (#is-parameter-reference-mod? @variable.parameter.chain_1 "8" "0")

(identifier) @variable.parameter.chain_2
  (#is-parameter-reference-mod? @variable.parameter.chain_2 "8" "1")

; ... chain_3 through chain_8
```

The `@variable.parameter` base patterns fire first and establish the fallback. The `chain_N` patterns are more specific captures of the same nodes, so Zed's highlight system (which takes the longest matching theme key) prefers them when the predicate passes.

These files also carry a set of destructuring-aware base patterns from upstream (`object_assignment_pattern`, `assignment_pattern`, `shorthand_property_identifier_pattern`) that the rainbow patterns do not currently cover. See [Destructured Parameters](#destructured-parameters).

### Rust

```scheme
(parameter
  (identifier) @variable.parameter.chain_1
  (#sibling-index-is-even? @variable.parameter.chain_1))

(parameter
  (identifier) @variable.parameter.chain_2
  (#sibling-index-is-odd? @variable.parameter.chain_2))
```

Rust uses `parameter` (not `required_parameter`) as the node kind, and has declaration-site patterns only: there are no `#is-parameter-reference-*?` patterns, so Rust parameter references in the body are not colored.

### Python

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

Python requires separate patterns for plain identifiers and `typed_parameter` nodes, and is likewise declaration-site only.

---

## Theme Resolution

Theme key matching in `HighlightMap::new` uses a longest-prefix algorithm: for a capture name like `variable.parameter.chain_1`, the theme key that shares the most dot-separated components wins. The matching is order-independent across components (any component of the key must appear somewhere in the capture name), but the winning key is the one with the most matching components.

This means:

| Capture | Theme keys present | Resolved to |
|---|---|---|
| `variable.parameter.chain_1` | `variable`, `variable.parameter`, `variable.parameter.chain_1` | `variable.parameter.chain_1` |
| `variable.parameter.chain_1` | `variable`, `variable.parameter` | `variable.parameter` |
| `variable.parameter` | `variable`, `variable.parameter` | `variable.parameter` |

Captures fall back gracefully to `variable.parameter` when the chain keys are absent, which is the graceful-degradation property that keeps the currently-broken predicates from being visible. It also means **defining the theme keys is a required step, not an optional one**: no bundled theme (`one`, `ayu`, `gruvbox`) defines `variable.parameter.chain_N`, and neither does the fork's `crates/theme/src/default_colors.rs`. The same is true of `property.chain_N`, which upstream's own queries emit.

---

## Edge Cases and Limitations

These describe the behavior of the predicate implementation as designed. They apply once the predicates are restored.

### Destructured Parameters

Parameters using object or array destructuring (e.g., `{ a, b }: Config`) are not colored per-parameter. The `find_parameter_identifier` function only extracts a single name node from each parameter node. For a destructured parameter, the binding contains multiple identifiers.

Behavior: the parameter slot still occupies its ordinal position and its slot color is assigned, but only the top-level binding pattern (if it is a plain identifier) is matched. Destructured sub-bindings (`a`, `b` inside `{ a, b }`) will not be colored with the parameter's chain color; they fall through to `@variable.parameter` or `@variable`.

**To fix:** Extend `find_parameter_identifier` to return all leaf identifiers within a destructuring pattern, and update `function_parameter_index` to map each leaf identifier name to the parent parameter's index.

### Rest Parameters

Rest parameters (`...args`) parse as `rest_pattern` containing an `identifier`. The `find_parameter_identifier` fallback (first `identifier` named child) finds this identifier, so `args` is assigned a chain color based on its ordinal slot. References to `args` in the body are also colored correctly via `is_parameter_reference`.

### Default Value Parameters

Parameters with defaults (`x = 0`) parse as `assignment_pattern` inside `required_parameter`. `find_parameter_identifier` tries the `pattern` field first, which should resolve to the identifier before the `=`. This should work in most cases, but verify against the actual tree-sitter grammar version in use.

### Single-Parameter Arrow Functions

Arrow functions with a single parameter and no parentheses (`x => x + 1`) use a different AST node: the `arrow_function` has a `parameter` field (a bare `identifier`), not a `formal_parameters` node. `function_parameter_index` searches for `formal_parameters` and returns `None` for this form.

Behavior: the parameter `x` declared this way does not receive a chain color at the declaration site (it only receives `@variable.parameter` from the `(arrow_function parameter: (identifier) @variable.parameter)` pattern). References to `x` inside the body also do not receive chain coloring.

**To fix:** Add a branch in `function_parameter_index` handling the case where the function node has a `parameter` field instead of `formal_parameters`.

### Nested Functions and Closures

The `is_parameter_reference` predicate walks up the AST and stops at the *nearest* enclosing function boundary. This is correct behavior: a reference inside a nested function or closure is scoped to that nested function, not the outer one. If an inner function declares its own parameter with the same name as an outer parameter, the inner declaration shadows the outer, and references inside the inner function are attributed to the inner parameter's index, which is correct.

If an inner function captures an outer parameter by reference (a closure capture, not a re-declaration), the walk stops at the inner function boundary, finds no matching parameter in its list, and returns `false`. The reference then falls through to `@variable.parameter` or `@variable`. This is a known limitation: closure captures of outer parameters are not colored with the outer parameter's chain color.

**To fix:** When `function_parameter_index` returns `None` for the innermost function, continue walking outward to look for the name in enclosing function parameter lists. This requires understanding JavaScript/TypeScript scoping rules, which is non-trivial to do accurately in a tree-sitter predicate without full semantic analysis.

### Name Matching Is Textual

Reference resolution matches on identifier text within the enclosing function, with no scope analysis beyond the function boundary. A local variable that shadows a parameter name inside a nested block still resolves to the parameter's color.

### TypeScript-Only: `this` Parameter

TypeScript allows a fake `this` parameter as the first formal parameter to annotate the type of `this`. This parameter occupies index 0 and receives `chain_1` coloring. This is generally acceptable but could be surprising. If undesired, filter it out in `function_parameter_index` by name.

### Methods with Implicit `self` (Rust, Python)

In Rust, `self` and `&self` are `self_parameter` nodes, not `parameter` nodes, so they are excluded from the sibling count automatically. In Python, `self` and `cls` are plain `identifier` nodes at index 0, so they receive `chain_1` coloring. Whether this is desired is a matter of preference.

---

## How to Extend to a New Language

1. **Determine the grammar node kinds** for the target language's function declarations and parameter lists. Use `tree-sitter parse` or the playground at `tree-sitter.github.io/tree-sitter/playground` to inspect the AST for representative code.

2. **Identify the parameter node kind**: the node that wraps each individual parameter within the list (e.g., `required_parameter` in TS, `parameter` in Rust, bare `identifier` or `typed_parameter` in Python).

3. **Write the declaration-site query patterns** in `crates/grammars/src/<language>/highlights.scm`. Use `#sibling-index-mod?` with a modulus of 8 for the full cycle, or `#sibling-index-is-even?` / `#sibling-index-is-odd?` for two-color alternation.

4. **Write the reference-site query patterns** using `#is-parameter-reference-mod?` (or the even/odd pair). Add an `(identifier)` capture with each predicate.

5. **Update `is_parameter_reference` in `syntax_map.rs`** to add the new language's function node kinds to the `match` arm. The existing set covers JavaScript, TypeScript, and Rust; Python, Go, and others need their own node kinds added.

6. **Update `function_parameter_index` in `syntax_map.rs`** to handle the new language's parameter node kind names in the `match child.kind()` block.

7. **Confirm the theme defines** as many `variable.parameter.chain_N` keys as the modulus you chose.

---

## Key Files

| File | Role | State |
|---|---|---|
| `crates/language/src/syntax_map.rs` | Custom predicate dispatch and implementation (`satisfies_custom_predicates`, `sibling_index_mod`, `sibling_index_parity`, `is_parameter_reference_mod`, `is_parameter_reference`, `function_parameter_index`, `find_parameter_identifier`) | Only `has_parent` remains |
| `crates/language/src/highlight_map.rs` | Theme key resolution via longest-prefix matching | Intact (upstream) |
| `crates/grammars/src/typescript/highlights.scm` | TS parameter chain query patterns (8-color) | Present |
| `crates/grammars/src/tsx/highlights.scm` | TSX parameter chain query patterns (8-color) | Present |
| `crates/grammars/src/javascript/highlights.scm` | JS parameter chain query patterns (8-color) | Present |
| `crates/grammars/src/rust/highlights.scm` | Rust parameter chain query patterns (2-color, declarations only) | Present |
| `crates/grammars/src/python/highlights.scm` | Python parameter chain query patterns (2-color, declarations only) | Present |
| `crates/theme/src/default_colors.rs` | Fork's syntax color defaults | No `chain_N` keys |

---

## Implementation Step-by-Step (Original Design)

This section documents the sequence in which the feature was built. It is also the order to work in when restoring it.

### Step 1: Add Custom Predicate Infrastructure

In the syntax highlight loop, intercept `general_predicates` on each query match. Implement a dispatch function mapping predicate operator strings to Rust functions. This is the extension point every subsequent step depends on.

Note the failure mode this fork hit: an unrecognized predicate falls through to `_ => true`. A predicate that silently disappears therefore makes every guarded pattern match unconditionally rather than erroring. If you want merges to surface this, make the fallback arm log once per unknown operator, or add a test asserting the dispatch table covers every predicate used across `crates/grammars/src/*/highlights.scm`.

### Step 2: Implement `sibling_index_mod`

Given a captured node, walk to its parent and grandparent and count the parent's index among named siblings, then compare against the modulus and remainder arguments. Sufficient to color parameters at their declaration site.

### Step 3: Add Declaration-Site Query Patterns

For each supported language, add `chain_N` capture patterns to `highlights.scm` with the sibling-index predicates. Verify in the editor that parameter declarations cycle colors.

### Step 4: Implement `is_parameter_reference_mod`

Resolve an identifier to a parameter declaration: walk up the AST to the enclosing function, locate the parameter list, and match identifier text against parameter names. This is the highest-complexity step and the only one needing rope access.

### Step 5: Add Reference-Site Query Patterns

Add `(identifier) @variable.parameter.chain_N (#is-parameter-reference-mod? ...)` for each slot. Verify that body references match their declaration colors.

### Step 6: Define Theme Keys

Add `variable.parameter.chain_1` through `chain_8` to the theme's syntax highlight list with distinct colors. Because `HighlightMap` uses longest-prefix matching, these keys automatically take priority over the base `variable.parameter` key when chain predicates match. Without this step, steps 1-5 produce no visible change.

### Step 7: Validate Edge Cases

Test destructuring, rest parameters, default values, single-param arrow functions, nested closures, and shadowing. Document any unhandled cases.
